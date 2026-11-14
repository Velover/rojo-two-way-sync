--[[
	Connect and disconnect from an attribute, for debugging and for agents that drive Studio over
	MCP and can't click Connect. Off unless the "Connect From An Attribute" setting is on.

	Set `ServerStorage.__Rojo_Connect` to a port ("34872"), an address ("localhost:34872" or
	"http://host:port"), or "disconnect". The plugin clears the attribute as soon as it reads it,
	so a request is never saved with the place. A connection made this way is accepted without
	the confirmation step, like play solo auto-connect. How it went is written to
	`ServerStorage.__Rojo_Status`: "connecting", "connected <project> <address>", "error <message>"
	or "disconnected".
]]

local ServerStorage = game:GetService("ServerStorage")

local Rojo = script:FindFirstAncestor("Rojo")
local Plugin = Rojo.Plugin

local Settings = require(Plugin.Settings)

local REQUEST = "__Rojo_Connect"
local STATUS = "__Rojo_Status"
local SETTING = "debugConnectFromAttribute"

local DebugConnect = {}

function DebugConnect.isEnabled(): boolean
	return Settings:get(SETTING) == true
end

-- Writes how the session is doing, while the setting is on.
function DebugConnect.report(text: string)
	if DebugConnect.isEnabled() then
		ServerStorage:SetAttribute(STATUS, text)
	end
end

-- "34872", "localhost:34872" or "http://host:34872" as a host (or nil) and a port.
function DebugConnect.parse(request: string): (string?, string?)
	local text = string.match(request, "^%s*(.-)%s*$")
	if string.match(text, "^%d+$") then
		return nil, text
	end
	local host, port = string.match(text, "^(.+):(%d+)$")
	return host, port
end

-- Watches for requests. `app` is Rojo's App. Returns a function that stops watching.
function DebugConnect.watch(app)
	local function handle()
		if not DebugConnect.isEnabled() then
			return
		end
		local request = ServerStorage:GetAttribute(REQUEST)
		if request == nil then
			return
		end
		ServerStorage:SetAttribute(REQUEST, nil)

		if string.lower(tostring(request)) == "disconnect" then
			app:endSession()
			DebugConnect.report("disconnected")
			return
		end
		local host, port = DebugConnect.parse(tostring(request))
		if not port then
			DebugConnect.report(
				"error " .. REQUEST .. " takes a port, host:port or disconnect, not " .. tostring(request)
			)
			return
		end
		app:endSession()
		app.setHost(host or "")
		app.setPort(port)
		app.connectedFromAttribute = true
		app:startSession()
	end

	local changed = ServerStorage:GetAttributeChangedSignal(REQUEST):Connect(handle)
	local stopListening = Settings:onChanged(SETTING, function(enabled)
		if enabled then
			handle()
		else
			ServerStorage:SetAttribute(STATUS, nil)
		end
	end)
	-- A request made before the plugin loaded.
	task.defer(handle)

	return function()
		changed:Disconnect()
		stopListening()
	end
end

return DebugConnect
