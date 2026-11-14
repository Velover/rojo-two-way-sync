--[[
	Two-way sync for the trees a project's twoway.json lists, next to Rojo's one-way sync.

	The server (Rojo with the twoway library) decides everything; this reports what Studio has and
	carries out what it's told. It starts when Rojo connects to a server that announces `twoway`
	in /api/rojo, and stops with Rojo's session.

	Open scripts are changed with ScriptDocument:EditTextAsync on only the lines that differ, because
	UpdateSourceAsync sends the cursor to the end of the file. Nothing here depends on timing: what
	was last agreed is kept in `known`, and an edit that would overwrite unsent typing is refused.
]]

local ChangeHistoryService = game:GetService("ChangeHistoryService")
local HttpService = game:GetService("HttpService")
local RunService = game:GetService("RunService")
local ScriptEditorService = game:GetService("ScriptEditorService")

local TwoWayStudio = require(script.Parent.TwoWayStudio)

local STATUS_SECONDS = 2
local MAX_NOTICES = 8
local TYPING_DEBOUNCE_SECONDS = 0.4
local SCAN_SECONDS = 1.5
local MODEL_SCAN_SECONDS = 5
local TAG = "[twoway]"
-- A local place's file name, read before Rojo renames the DataModel to the project's name.
local PLACE_NAME = game.Name

local session = nil
local mappings = {}
local applying = false
local dirty = false
local last_typed = 0
-- Each start gets a new generation; loops from an older one stop by themselves.
local generation = 0
local warned = {}
local notices = {}
local last_error = nil
local server_status = nil
-- Where the sync is served, such as "http://localhost:34872/twoway".
local base_url = nil
local status_callback = nil
local typing_connection = nil

local function Refresh()
	if status_callback then
		local mapping_list = {}
		for _, mapping in mappings do
			table.insert(mapping_list, { studio = mapping.studio, auto = mapping.auto, group = mapping.group })
		end
		status_callback({
			active = session ~= nil,
			mappings = mapping_list,
			conflicts = server_status and server_status.conflicts or 0,
			warnings = server_status and server_status.warnings or {},
			notices = notices,
			error = last_error,
		})
	end
end

local function WarnOnce(message)
	if not warned[message] then
		warned[message] = true
		warn(TAG, message)
		table.insert(notices, 1, message)
		if #notices > MAX_NOTICES then
			table.remove(notices)
		end
		Refresh()
	end
end

local function Request(method, path, body, raw)
	local ok, response = pcall(function()
		return HttpService:RequestAsync({
			Url = base_url .. path,
			Method = method,
			Headers = { ["Content-Type"] = raw and "application/octet-stream" or "application/json" },
			Body = raw or (body and HttpService:JSONEncode(body)) or nil,
		})
	end)
	if ok then
		return response
	end
	return nil, response
end

local function Decode(response)
	local ok, value = pcall(function()
		return HttpService:JSONDecode(response.Body)
	end)
	return ok and value or nil
end

local function SessionQuery(extra)
	return "?session=" .. HttpService:UrlEncode(session) .. (extra or "")
end

---------------------------------------------------------------------------------------------------
-- Reading Studio

-- Sends what changed since the last report.
local function Flush(mapping)
	local current = TwoWayStudio.scan(mapping)
	local upserts, deletes = {}, {}
	for key, entry in current do
		local previous = mapping.known[key]
		if not previous or not TwoWayStudio.same(previous, entry) then
			table.insert(upserts, entry)
		end
	end
	for key in mapping.known do
		if not current[key] then
			table.insert(deletes, key)
		end
	end
	if #upserts == 0 and #deletes == 0 then
		return true
	end
	local query = SessionQuery("&mapping=" .. mapping.index)
	local response = Request("POST", "/api/changes" .. query, { upserts = upserts, deletes = deletes })
	if response and response.Success then
		mapping.known = current
		return true
	end
	return false
end

local function FlushModels(mapping)
	for _, key in mapping.modelKeys or {} do
		local instance = TwoWayStudio.isGroupKey(key) and TwoWayStudio.find(mapping, TwoWayStudio.parentKey(key))
			or TwoWayStudio.find(mapping, key)
		if instance then
			local bytes = TwoWayStudio.serializeKey(mapping, key)
			if not bytes and not TwoWayStudio.isModel(mapping, key) then
				-- Found by auto mode: leave it out from now on rather than hold up the rest.
				mapping.unsavable[key] = true
			end
			if bytes and bytes ~= mapping.modelBytes[key] then
				local query = SessionQuery("&mapping=" .. mapping.index .. "&key=" .. HttpService:UrlEncode(key))
				local response = Request("POST", "/api/model" .. query, nil, bytes)
				if response and response.Success then
					mapping.modelBytes[key] = bytes
				end
			end
		end
	end
end

---------------------------------------------------------------------------------------------------
-- Carrying out the server's ops

local function ApplyOp(op)
	local mapping = mappings[op.mapping + 1]
	local recording = nil
	if op.kind ~= "script" or op.action == "delete" then
		local ok, id = pcall(function()
			return ChangeHistoryService:TryBeginRecording("twoway sync")
		end)
		recording = ok and id or nil
	end

	local ok, done, kept = pcall(function()
		if op.action == "delete" then
			return TwoWayStudio.applyDelete(mapping, op)
		end
		local blob = nil
		if op.kind == "model" then
			local response = Request("GET", "/api/blob/" .. op.blob)
			if not response or not response.Success then
				error("could not fetch the model")
			end
			blob = response.Body
		end
		return TwoWayStudio.applyUpsert(mapping, op, blob, Flush), false
	end)
	if recording then
		ChangeHistoryService:FinishRecording(recording, Enum.FinishRecordingOperation.Commit)
	end
	if not ok then
		warn(TAG, "could not apply", op.key, done)
		done = false
	end
	Request("POST", "/api/ack" .. SessionQuery(), { op = op.id, ok = done == true, kept = kept == true })
end

---------------------------------------------------------------------------------------------------
-- The session

local function Disconnect(message)
	if session then
		print(TAG, message or "disconnected")
	end
	session = nil
	mappings = {}
	Refresh()
end

local function Connect()
	local response, failure = Request("POST", "/api/connect", {
		placeId = game.PlaceId,
		placeName = PLACE_NAME,
	})
	if not response then
		last_error = "The two-way sync didn't answer: " .. tostring(failure)
		warn(TAG, last_error)
		Refresh()
		return
	end
	local body = Decode(response)
	if not response.Success or not body then
		last_error = body and body.error or response.StatusMessage
		warn(TAG, last_error)
		Refresh()
		return
	end
	last_error = nil

	session = body.session
	mappings = {}
	for _, info in body.mappings do
		table.insert(mappings, TwoWayStudio.newMapping(info))
	end

	for _, mapping in mappings do
		-- The scan finds the models; their bytes go up before the snapshot that lists them.
		local current, models = TwoWayStudio.snapshot(mapping)
		for key, bytes in models do
			local query = SessionQuery("&mapping=" .. mapping.index .. "&key=" .. HttpService:UrlEncode(key))
			local uploaded = Request("POST", "/api/model" .. query, nil, bytes)
			if uploaded and uploaded.Success then
				mapping.modelBytes[key] = bytes
			end
		end
		local entries = {}
		for _, entry in current do
			table.insert(entries, entry)
		end
		local sent =
			Request("POST", "/api/snapshot" .. SessionQuery("&mapping=" .. mapping.index), { entries = entries })
		if not sent or not sent.Success then
			Disconnect("the first sync failed")
			return
		end
		mapping.known = current
	end
	print(TAG, "connected:", #mappings, "mapping(s)")
	Refresh()
end

local TwoWay = {}

-- Starts the sync. `options.baseUrl` is where the server mounts it; `options.onStatus` is called
-- with a summary whenever something worth showing changes.
function TwoWay.start(options)
	TwoWay.stop()
	TwoWayStudio.onWarn = WarnOnce
	if not RunService:IsEdit() then
		return
	end
	generation += 1
	local mine = generation
	local function Current()
		return generation == mine
	end

	base_url = options.baseUrl
	status_callback = options.onStatus
	warned = {}
	notices = {}
	last_error = nil
	server_status = nil

	typing_connection = ScriptEditorService.TextDocumentDidChange:Connect(function()
		if session then
			dirty = true
			last_typed = os.clock()
		end
	end)

	task.spawn(function()
		Connect()

		-- The server's summary, for Rojo's connected page.
		task.spawn(function()
			while Current() and session do
				local response = Request("GET", "/api/status")
				server_status = response and response.Success and Decode(response) or nil
				Refresh()
				task.wait(STATUS_SECONDS)
			end
		end)

		-- Waits for the server's ops.
		task.spawn(function()
			while Current() and session do
				local current = session
				local response = Request("GET", "/api/poll" .. SessionQuery())
				if Current() and session == current then
					if response and response.StatusCode == 410 then
						-- Only one Studio syncs both ways at a time: the latest to connect.
						last_error =
							"Another Studio connected, so the two-way sync moved there. Reconnect to take it back."
						Disconnect(last_error)
					elseif not response then
						Disconnect("lost the server")
					elseif response.Success then
						local body = Decode(response)
						if body and #body.ops > 0 then
							applying = true
							for _, op in body.ops do
								if Current() and session == current then
									ApplyOp(op)
								end
							end
							applying = false
						end
					end
				end
			end
		end)

		-- Reports Studio's changes: soon after typing stops, and on a slow tick for everything else.
		local last_scan, last_model_scan = 0, 0
		while Current() and session do
			task.wait(0.1)
			if session and not applying then
				local now = os.clock()
				local typed = dirty and now - last_typed >= TYPING_DEBOUNCE_SECONDS
				if typed or now - last_scan >= SCAN_SECONDS then
					dirty = false
					last_scan = now
					for _, mapping in mappings do
						if session then
							Flush(mapping)
						end
					end
				end
				if now - last_model_scan >= MODEL_SCAN_SECONDS then
					last_model_scan = now
					for _, mapping in mappings do
						if session then
							FlushModels(mapping)
						end
					end
				end
			end
		end
	end)
end

-- Ends the sync's session, if any.
function TwoWay.stop()
	generation += 1
	if typing_connection then
		typing_connection:Disconnect()
		typing_connection = nil
	end
	if session and base_url then
		local query = SessionQuery()
		task.spawn(Request, "POST", "/api/disconnect" .. query)
	end
	Disconnect()
	status_callback = nil
end

-- Plugins can't open a browser, so the server does.
function TwoWay.openReviewPage()
	if base_url then
		task.spawn(Request, "POST", "/api/open-review")
	end
end

return TwoWay
