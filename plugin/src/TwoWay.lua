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
local SerializationService = game:GetService("SerializationService")

local STATUS_SECONDS = 2
local MAX_NOTICES = 8
local TYPING_DEBOUNCE_SECONDS = 0.4
local SCAN_SECONDS = 1.5
local MODEL_SCAN_SECONDS = 5
local MAX_DIFF_CELLS = 2000000
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
local daemon_status = nil
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
			conflicts = daemon_status and daemon_status.conflicts or 0,
			warnings = daemon_status and daemon_status.warnings or {},
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

local function ScriptTypeOf(instance)
	if instance:IsA("ModuleScript") then
		return "module"
	elseif instance:IsA("LocalScript") then
		return "client"
	elseif instance:IsA("Script") then
		if instance.RunContext == Enum.RunContext.Client then
			return "client"
		elseif instance.RunContext == Enum.RunContext.Plugin then
			return "plugin"
		end
		return "server"
	end
	return nil
end

-- Attribute values, encoded as in a Rojo meta file: plain strings, booleans and numbers, and
-- `{ Type = ... }` for everything else.
local function EncodeValue(value)
	local kind = typeof(value)
	if kind == "string" or kind == "boolean" or kind == "number" then
		return value
	elseif kind == "Vector3" then
		return { Vector3 = { value.X, value.Y, value.Z } }
	elseif kind == "Vector2" then
		return { Vector2 = { value.X, value.Y } }
	elseif kind == "Color3" then
		return { Color3 = { value.R, value.G, value.B } }
	elseif kind == "UDim" then
		return { UDim = { value.Scale, value.Offset } }
	elseif kind == "UDim2" then
		return { UDim2 = { { value.X.Scale, value.X.Offset }, { value.Y.Scale, value.Y.Offset } } }
	elseif kind == "NumberRange" then
		return { NumberRange = { value.Min, value.Max } }
	elseif kind == "Rect" then
		return { Rect = { { value.Min.X, value.Min.Y }, { value.Max.X, value.Max.Y } } }
	elseif kind == "BrickColor" then
		return { BrickColor = value.Number }
	elseif kind == "CFrame" then
		local x, y, z, r00, r01, r02, r10, r11, r12, r20, r21, r22 = value:GetComponents()
		return {
			CFrame = {
				position = { x, y, z },
				orientation = { { r00, r01, r02 }, { r10, r11, r12 }, { r20, r21, r22 } },
			},
		}
	elseif kind == "NumberSequence" then
		local points = {}
		for _, point in value.Keypoints do
			table.insert(points, { time = point.Time, value = point.Value, envelope = point.Envelope })
		end
		return { NumberSequence = { keypoints = points } }
	elseif kind == "ColorSequence" then
		local points = {}
		for _, point in value.Keypoints do
			table.insert(points, { time = point.Time, color = { point.Value.R, point.Value.G, point.Value.B } })
		end
		return { ColorSequence = { keypoints = points } }
	elseif kind == "Font" then
		return { Font = { family = value.Family, weight = value.Weight.Name, style = value.Style.Name } }
	elseif kind == "EnumItem" then
		return { EnumItem = { type = tostring(value.EnumType), value = value.Value } }
	end
	return nil
end

local function DecodeValue(value)
	if type(value) ~= "table" then
		return value
	end
	local name, data = next(value)
	if type(data) ~= "table" then
		-- Explicit simple types, such as { Float64 = 1 } or { BrickColor = 194 }.
		if name == "BrickColor" then
			return BrickColor.new(data)
		end
		return data
	elseif name == "Vector3" then
		return Vector3.new(data[1], data[2], data[3])
	elseif name == "Vector2" then
		return Vector2.new(data[1], data[2])
	elseif name == "Color3" then
		return Color3.new(data[1], data[2], data[3])
	elseif name == "UDim" then
		return UDim.new(data[1], data[2])
	elseif name == "UDim2" then
		return UDim2.new(data[1][1], data[1][2], data[2][1], data[2][2])
	elseif name == "NumberRange" then
		return NumberRange.new(data[1], data[2])
	elseif name == "Rect" then
		return Rect.new(data[1][1], data[1][2], data[2][1], data[2][2])
	elseif name == "CFrame" then
		local p, o = data.position, data.orientation
		return CFrame.new(
			p[1],
			p[2],
			p[3],
			o[1][1],
			o[1][2],
			o[1][3],
			o[2][1],
			o[2][2],
			o[2][3],
			o[3][1],
			o[3][2],
			o[3][3]
		)
	elseif name == "NumberSequence" then
		local points = {}
		for _, point in data.keypoints do
			table.insert(points, NumberSequenceKeypoint.new(point.time, point.value, point.envelope or 0))
		end
		return NumberSequence.new(points)
	elseif name == "ColorSequence" then
		local points = {}
		for _, point in data.keypoints do
			table.insert(
				points,
				ColorSequenceKeypoint.new(point.time, Color3.new(point.color[1], point.color[2], point.color[3]))
			)
		end
		return ColorSequence.new(points)
	elseif name == "Font" then
		return Font.new(data.family, Enum.FontWeight[data.weight or "Regular"], Enum.FontStyle[data.style or "Normal"])
	elseif name == "EnumItem" then
		return Enum[data.type]:FromValue(data.value)
	end
	return nil
end

local function DeepEqual(a, b)
	if type(a) ~= "table" or type(b) ~= "table" then
		return a == b
	end
	for key, value in a do
		if not DeepEqual(value, b[key]) then
			return false
		end
	end
	for key in b do
		if a[key] == nil then
			return false
		end
	end
	return true
end

local function ReadMeta(instance)
	local attributes = {}
	for name, value in instance:GetAttributes() do
		local encoded = EncodeValue(value)
		if encoded == nil then
			WarnOnce(instance:GetFullName() .. "." .. name .. " is a " .. typeof(value) .. ", which can't be saved")
		else
			attributes[name] = encoded
		end
	end
	local tags = instance:GetTags()
	table.sort(tags)
	return attributes, tags
end

-- Makes an instance's attributes and tags exactly these.
local function ApplyMeta(instance, attributes, tags)
	if attributes == nil then
		return
	end
	for name in instance:GetAttributes() do
		if attributes[name] == nil then
			instance:SetAttribute(name, nil)
		end
	end
	for name, encoded in attributes do
		local ok, value = pcall(DecodeValue, encoded)
		if ok and value ~= nil then
			local set, problem = pcall(function()
				instance:SetAttribute(name, value)
			end)
			if not set then
				WarnOnce("couldn't set " .. instance:GetFullName() .. "." .. name .. ": " .. tostring(problem))
			end
		else
			WarnOnce("couldn't read the value of " .. instance:GetFullName() .. "." .. name)
		end
	end
	local wanted = {}
	for _, tag in tags or {} do
		wanted[tag] = true
	end
	for _, tag in instance:GetTags() do
		if not wanted[tag] then
			instance:RemoveTag(tag)
		end
	end
	for tag in wanted do
		if not instance:HasTag(tag) then
			instance:AddTag(tag)
		end
	end
end

local function Split(path)
	local parts = {}
	for part in string.gmatch(path, "[^/]+") do
		table.insert(parts, part)
	end
	return parts
end

-- A `models` entry can be a pattern: `*` is any one name, `**` any depth.
local function GlobToPattern(glob)
	local out = {}
	local i = 1
	while i <= #glob do
		local c = string.sub(glob, i, i)
		if c == "*" and string.sub(glob, i + 1, i + 1) == "*" then
			table.insert(out, ".*")
			i += 2
			continue
		elseif c == "*" then
			table.insert(out, "[^/]*")
		elseif c == "?" then
			table.insert(out, "[^/]")
		elseif string.find(c, "[%^%$%(%)%%%.%[%]%+%-]") then
			table.insert(out, "%" .. c)
		else
			table.insert(out, c)
		end
		i += 1
	end
	return "^" .. table.concat(out) .. "$"
end

local function MatchList(set, patterns, key)
	if set[key] then
		return true
	end
	for _, pattern in patterns do
		if string.match(key, pattern) then
			return true
		end
	end
	return false
end

local function IsExcluded(mapping, key)
	return MatchList(mapping.excludeSet, mapping.excludePatterns, key)
end

-- Listed in `models`: saved whole, scripts and all.
local function IsModel(mapping, key)
	return not IsExcluded(mapping, key) and MatchList(mapping.modelSet, mapping.modelPatterns, key)
end

-- Splits a list of paths into exact names and patterns.
local function CompileList(list)
	local set, patterns = {}, {}
	for _, key in list or {} do
		if string.find(key, "[%*%?]") then
			table.insert(patterns, GlobToPattern(key))
		else
			set[key] = true
		end
	end
	return set, patterns
end

local function JoinKey(parent, name)
	return parent == "" and name or (parent .. "/" .. name)
end

-- A folder's loose instances (not scripts or Folders) share one file, `init.rbxmx`, under the key
-- `<folder>/init`. No instance has that key: `init` is never a synced name.
local GROUP_NAME = "init"

local function IsGroupKey(key)
	return key == GROUP_NAME or string.sub(key, -(#GROUP_NAME + 1)) == "/" .. GROUP_NAME
end

local function ParentKey(key)
	return string.match(key, "^(.*)/[^/]*$") or ""
end

local function GroupsUnder(mapping, container_key)
	return mapping.auto
		and mapping.group
		and not MatchList(mapping.separateSet, mapping.separatePatterns, container_key)
end

local function IsContainer(instance)
	return instance:IsA("Folder") or instance:IsA("LuaSourceContainer")
end

-- An instance saved with its folder's others, rather than on its own.
local function IsLoose(mapping, child, child_key)
	return not IsContainer(child)
		and child.Archivable
		and not IsExcluded(mapping, child_key)
		and not IsModel(mapping, child_key)
end

local function FindRoot(mapping)
	local parts = Split(mapping.studio)
	local ok, current = pcall(function()
		return game:GetService(parts[1])
	end)
	if not ok or not current then
		current = game:FindFirstChild(parts[1])
	end
	for i = 2, #parts do
		current = current and current:FindFirstChild(parts[i])
	end
	return current
end

local function Find(mapping, key)
	local current = FindRoot(mapping)
	if key == "" then
		return current
	end
	for _, name in Split(key) do
		current = current and current:FindFirstChild(name)
	end
	return current
end

-- Scripts and folders under the root, plus models: the listed ones, or in auto mode every other
-- instance. Anything else isn't ours, and is neither read nor touched.
local function Walk(mapping, instance, key, out)
	local script_type = ScriptTypeOf(instance)
	local attributes, tags = ReadMeta(instance)
	if script_type then
		out[key] = {
			key = key,
			kind = "script",
			scriptType = script_type,
			source = ScriptEditorService:GetEditorSource(instance),
			attributes = attributes,
			tags = tags,
		}
	else
		out[key] = { key = key, kind = "folder", attributes = attributes, tags = tags }
	end

	local seen = {}
	local grouping = GroupsUnder(mapping, key)
	local grouped = false
	for _, child in instance:GetChildren() do
		local child_key = JoinKey(key, child.Name)
		if grouping and IsLoose(mapping, child, child_key) then
			-- In the folder's shared file; names may repeat there.
			grouped = true
			continue
		end
		local is_model = IsModel(mapping, child_key)
		local is_container = child:IsA("Folder") or child:IsA("LuaSourceContainer")
		if not is_model and not is_container and mapping.auto and not IsExcluded(mapping, child_key) then
			-- Instances that aren't saved with the place, or that Studio can't save, stay out.
			is_model = child.Archivable and not mapping.unsavable[child_key]
		end
		if is_container and IsExcluded(mapping, child_key) then
			is_container = false
		end
		if is_model or is_container then
			if seen[child.Name] then
				WarnOnce(child:GetFullName() .. " shares its name with a sibling and isn't synced")
			else
				seen[child.Name] = true
				if is_model then
					out[child_key] = { key = child_key, kind = "model" }
					table.insert(mapping.modelKeys, child_key)
				else
					Walk(mapping, child, child_key, out)
				end
			end
		end
	end

	local group_key = JoinKey(key, GROUP_NAME)
	if grouped and not mapping.unsavable[group_key] then
		out[group_key] = { key = group_key, kind = "model", group = true }
		table.insert(mapping.modelKeys, group_key)
	end
end

-- The instances in a folder's shared file, in Studio's order.
local function GroupMembers(mapping, container_key)
	local container = Find(mapping, container_key)
	local members = {}
	if container then
		for _, child in container:GetChildren() do
			if IsLoose(mapping, child, JoinKey(container_key, child.Name)) then
				table.insert(members, child)
			end
		end
	end
	return members, container
end

local function ScanMapping(mapping)
	local out = {}
	mapping.modelKeys = {}
	local root = FindRoot(mapping)
	if root then
		Walk(mapping, root, "", out)
	end
	return out
end

local function Same(a, b)
	return a.kind == b.kind
		and a.scriptType == b.scriptType
		and a.source == b.source
		and DeepEqual(a.attributes or {}, b.attributes or {})
		and DeepEqual(a.tags or {}, b.tags or {})
end

-- Sends what changed since the last report.
local function Flush(mapping)
	local current = ScanMapping(mapping)
	local upserts, deletes = {}, {}
	for key, entry in current do
		local previous = mapping.known[key]
		if not previous or not Same(previous, entry) then
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

local function SerializeList(list, label)
	local ok, bytes = pcall(function()
		return buffer.tostring(SerializationService:SerializeInstancesAsync(list))
	end)
	if ok then
		return bytes
	end
	WarnOnce("could not save " .. label .. ": " .. tostring(bytes))
	return nil
end

local function SerializeModel(instance)
	return SerializeList({ instance }, instance:GetFullName())
end

-- The bytes of a model, or of a folder's shared file.
local function SerializeKey(mapping, key)
	if IsGroupKey(key) then
		local members, container = GroupMembers(mapping, ParentKey(key))
		if #members == 0 then
			return nil
		end
		return SerializeList(members, container:GetFullName() .. " (its instances)")
	end
	local instance = Find(mapping, key)
	return instance and SerializeModel(instance)
end

local function FlushModels(mapping)
	for _, key in mapping.modelKeys or {} do
		local instance = IsGroupKey(key) and Find(mapping, ParentKey(key)) or Find(mapping, key)
		if instance then
			local bytes = SerializeKey(mapping, key)
			if not bytes and not IsModel(mapping, key) then
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
-- Changing an open script without moving the cursor

local function SplitLines(text)
	local lines = {}
	local start = 1
	while true do
		local stop = string.find(text, "\n", start, true)
		if not stop then
			table.insert(lines, string.sub(text, start))
			return lines
		end
		table.insert(lines, string.sub(text, start, stop - 1))
		start = stop + 1
	end
end

-- The changed line ranges, top to bottom: old lines [a, b) became new lines [c, d).
local function DiffLines(old, new)
	local top = 0
	while top < #old and top < #new and old[top + 1] == new[top + 1] do
		top += 1
	end
	local tail = 0
	while tail < #old - top and tail < #new - top and old[#old - tail] == new[#new - tail] do
		tail += 1
	end

	local n, m = #old - top - tail, #new - top - tail
	if n == 0 and m == 0 then
		return {}
	end
	if n == 0 or m == 0 or n * m > MAX_DIFF_CELLS then
		return { { a = top + 1, b = top + n + 1, c = top + 1, d = top + m + 1 } }
	end

	-- Longest common subsequence over the part that differs.
	local width = m + 1
	local cells = buffer.create((n + 1) * width * 4)
	for i = n - 1, 0, -1 do
		for j = m - 1, 0, -1 do
			local value
			if old[top + i + 1] == new[top + j + 1] then
				value = buffer.readu32(cells, ((i + 1) * width + j + 1) * 4) + 1
			else
				value = math.max(
					buffer.readu32(cells, ((i + 1) * width + j) * 4),
					buffer.readu32(cells, (i * width + j + 1) * 4)
				)
			end
			buffer.writeu32(cells, (i * width + j) * 4, value)
		end
	end

	local hunks = {}
	local i, j = 0, 0
	local open = nil
	local function Close()
		if open then
			open.b = top + i + 1
			open.d = top + j + 1
			table.insert(hunks, open)
			open = nil
		end
	end
	while i < n or j < m do
		if i < n and j < m and old[top + i + 1] == new[top + j + 1] then
			Close()
			i += 1
			j += 1
		else
			open = open or { a = top + i + 1, c = top + j + 1 }
			local down = i < n and buffer.readu32(cells, ((i + 1) * width + j) * 4) or -1
			local right = j < m and buffer.readu32(cells, (i * width + j + 1) * 4) or -1
			if down >= right then
				i += 1
			else
				j += 1
			end
		end
	end
	Close()
	return hunks
end

local function CommonPrefix(a, b)
	local count = 0
	local limit = math.min(#a, #b)
	while count < limit and string.byte(a, count + 1) == string.byte(b, count + 1) do
		count += 1
	end
	-- Don't stop inside a UTF-8 character.
	while count > 0 and (string.byte(a, count + 1) or 0) >= 0x80 and (string.byte(a, count + 1) or 0) < 0xC0 do
		count -= 1
	end
	return count
end

local function CommonSuffix(a, b, skip)
	local count = 0
	local limit = math.min(#a, #b) - skip
	while count < limit and string.byte(a, #a - count) == string.byte(b, #b - count) do
		count += 1
	end
	while
		count > 0
		and (string.byte(a, #a - count + 1) or 0) >= 0x80
		and (string.byte(a, #a - count + 1) or 0) < 0xC0
	do
		count -= 1
	end
	return count
end

-- Editor columns count bytes, and only positions between characters are valid.
local function Column(bytes)
	return bytes + 1
end

local function EditHunk(document, old, new, hunk)
	local replacement = table.concat(new, "\n", hunk.c, hunk.d - 1)
	local count = hunk.d - hunk.c
	local removed = hunk.b - hunk.a

	if removed == 1 and count == 1 then
		-- One line to one line: only the part inside it that changed.
		local before, after = old[hunk.a], new[hunk.c]
		local prefix = CommonPrefix(before, after)
		local suffix = CommonSuffix(before, after, prefix)
		local middle = string.sub(after, prefix + 1, #after - suffix)
		return document:EditTextAsync(middle, hunk.a, Column(prefix), hunk.a, Column(#before - suffix))
	elseif hunk.b <= #old then
		-- Whole lines, with something after them.
		local text = count > 0 and (replacement .. "\n") or ""
		return document:EditTextAsync(text, hunk.a, 1, hunk.b, 1)
	elseif hunk.a > 1 then
		-- Reaches the end of the file, so the line break before it goes or comes too.
		local previous = old[hunk.a - 1]
		local last = old[#old]
		local text = count > 0 and ("\n" .. replacement) or ""
		return document:EditTextAsync(text, hunk.a - 1, Column(#previous), #old, Column(#last))
	end
	local last = old[#old]
	return document:EditTextAsync(replacement, 1, 1, #old, Column(#last))
end

local function WholeWrite(instance, text)
	ScriptEditorService:UpdateSourceAsync(instance, function()
		return text
	end)
end

local function ApplySource(instance, text)
	local document = ScriptEditorService:FindScriptDocument(instance)
	if not document then
		WholeWrite(instance, text)
		return
	end

	local old = SplitLines(document:GetText())
	local new = SplitLines(text)
	local hunks = DiffLines(old, new)
	local edited = pcall(function()
		-- Bottom to top, so a hunk's line numbers stay right.
		for i = #hunks, 1, -1 do
			local ok, message = EditHunk(document, old, new, hunks[i])
			if not ok then
				error(message)
			end
		end
	end)

	if not edited or document:GetText() ~= text then
		-- The safety net: the text must end up right, even at the cost of the cursor.
		local a, b, c, d = document:GetSelection()
		WholeWrite(instance, text)
		pcall(function()
			document:RequestSetSelectionAsync(a, b, c, d)
		end)
		WarnOnce("had to rewrite " .. instance:GetFullName() .. " in full")
	end
end

---------------------------------------------------------------------------------------------------
-- Carrying out the daemon's ops

local function EnsureParent(mapping, key)
	local parts = Split(mapping.studio)
	for _, name in Split(key) do
		table.insert(parts, name)
	end
	table.remove(parts)

	local ok, current = pcall(function()
		return game:GetService(parts[1])
	end)
	if not ok or not current then
		error("no service " .. tostring(parts[1]))
	end
	for i = 2, #parts do
		local child = current:FindFirstChild(parts[i])
		if not child then
			child = Instance.new("Folder")
			child.Name = parts[i]
			child.Parent = current
		end
		current = child
	end
	return current
end

local function NameOf(mapping, key)
	local parts = Split(key == "" and mapping.studio or key)
	return parts[#parts]
end

-- Swaps an instance for one of another class, keeping its children.
local function Replace(old, new)
	for _, child in old:GetChildren() do
		child.Parent = new
	end
	new.Parent = old.Parent
	old:Destroy()
end

local function ApplyUpsert(mapping, op)
	local instance = Find(mapping, op.key)

	if op.kind == "folder" then
		if not instance then
			instance = Instance.new("Folder")
			instance.Name = NameOf(mapping, op.key)
			instance.Parent = EnsureParent(mapping, op.key)
		elseif ScriptTypeOf(instance) then
			local folder = Instance.new("Folder")
			folder.Name = instance.Name
			Replace(instance, folder)
			instance = folder
		end
		ApplyMeta(instance, op.attributes, op.tags)
		mapping.known[op.key] = { key = op.key, kind = "folder", attributes = op.attributes, tags = op.tags }
		return true
	end

	if op.kind == "model" and IsGroupKey(op.key) then
		local response = Request("GET", "/api/blob/" .. op.blob)
		if not response or not response.Success then
			error("could not fetch the model")
		end
		local created = SerializationService:DeserializeInstancesAsync(buffer.fromstring(response.Body))
		local container = EnsureParent(mapping, op.key)
		local old = GroupMembers(mapping, ParentKey(op.key))
		for _, member in old do
			member:Destroy()
		end
		for _, member in created do
			member.Parent = container
		end
		mapping.known[op.key] = { key = op.key, kind = "model", group = true }
		mapping.modelBytes[op.key] = SerializeKey(mapping, op.key)
		return true
	end

	if op.kind == "model" then
		local response = Request("GET", "/api/blob/" .. op.blob)
		if not response or not response.Success then
			error("could not fetch the model")
		end
		local created = SerializationService:DeserializeInstancesAsync(buffer.fromstring(response.Body))
		local model = created[1]
		model.Name = NameOf(mapping, op.key)
		model.Parent = instance and instance.Parent or EnsureParent(mapping, op.key)
		if instance then
			instance:Destroy()
		end
		mapping.known[op.key] = { key = op.key, kind = "model" }
		mapping.modelBytes[op.key] = SerializeModel(model)
		return true
	end

	local entry = { key = op.key, kind = "script", source = op.source, attributes = op.attributes, tags = op.tags }
	local same_family = instance and ScriptTypeOf(instance) == op.scriptType
	-- The daemon speaks Script Sync's names too: they're the same families.
	if op.scriptType == "local" or op.scriptType == "legacy" then
		same_family = instance and ScriptTypeOf(instance) == (op.scriptType == "local" and "client" or "server")
	end

	if same_family then
		local editor = ScriptEditorService:GetEditorSource(instance)
		local previous = mapping.known[op.key]
		if editor ~= op.source then
			if previous and previous.source ~= editor then
				-- Typed since the last report: don't overwrite it. It goes to the daemon, which
				-- sees both edits and asks.
				Flush(mapping)
				return false
			end
			ApplySource(instance, op.source)
		end
		ApplyMeta(instance, op.attributes, op.tags)
		entry.scriptType = ScriptTypeOf(instance)
	else
		local script = Instance.new(op.className)
		script.Name = NameOf(mapping, op.key)
		if op.runContext ~= nil and script:IsA("Script") then
			for _, item in Enum.RunContext:GetEnumItems() do
				if item.Value == op.runContext then
					script.RunContext = item
				end
			end
		end
		script.Source = op.source
		ApplyMeta(script, op.attributes, op.tags)
		if instance then
			Replace(instance, script)
		else
			script.Parent = EnsureParent(mapping, op.key)
		end
		entry.scriptType = ScriptTypeOf(script)
	end
	mapping.known[op.key] = entry
	return true
end

local function ApplyDelete(mapping, op)
	if IsGroupKey(op.key) then
		for _, member in GroupMembers(mapping, ParentKey(op.key)) do
			member:Destroy()
		end
		mapping.known[op.key] = nil
		mapping.modelBytes[op.key] = nil
		return true
	end
	local instance = Find(mapping, op.key)
	if instance then
		-- Whatever is still inside isn't synced, so it isn't ours to remove.
		local known = mapping.known[op.key]
		local is_model = IsModel(mapping, op.key) or (known ~= nil and known.kind == "model")
		if not is_model and #instance:GetChildren() > 0 then
			return false, true
		end
		local parent = instance.Parent
		instance:Destroy()
		-- A Folder left empty synced only because of what it held, so it goes too.
		local root = FindRoot(mapping)
		while
			parent
			and parent ~= root
			and parent:IsA("Folder")
			and #parent:GetChildren() == 0
			and next(parent:GetAttributes()) == nil
			and #parent:GetTags() == 0
		do
			local above = parent.Parent
			parent:Destroy()
			parent = above
		end
	end
	mapping.known[op.key] = nil
	mapping.modelBytes[op.key] = nil
	return true
end

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
			return ApplyDelete(mapping, op)
		end
		return ApplyUpsert(mapping, op), false
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

local function Connect(manual)
	local response, failure = Request("POST", "/api/connect", {
		placeId = game.PlaceId,
		placeName = PLACE_NAME,
		manual = manual,
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
		local set, patterns = CompileList(info.models)
		local exclude_set, exclude_patterns = CompileList(info.exclude)
		local separate_set, separate_patterns = CompileList(info.separate)
		table.insert(mappings, {
			index = info.index,
			studio = info.studio,
			models = info.models,
			auto = info.auto == true,
			modelSet = set,
			modelPatterns = patterns,
			excludeSet = exclude_set,
			excludePatterns = exclude_patterns,
			group = info.group == true,
			separateSet = separate_set,
			separatePatterns = separate_patterns,
			modelKeys = {},
			known = {},
			modelBytes = {},
			unsavable = {},
		})
	end

	for _, mapping in mappings do
		-- The scan finds the models; their bytes go up before the snapshot that lists them.
		local current = ScanMapping(mapping)
		FlushModels(mapping)
		for key in mapping.unsavable do
			current[key] = nil
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
	daemon_status = nil

	typing_connection = ScriptEditorService.TextDocumentDidChange:Connect(function()
		if session then
			dirty = true
			last_typed = os.clock()
		end
	end)

	task.spawn(function()
		Connect(true)

		-- The server's summary, for Rojo's connected page.
		task.spawn(function()
			while Current() and session do
				local response = Request("GET", "/api/status")
				daemon_status = response and response.Success and Decode(response) or nil
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
						Disconnect("the server ended the session")
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
