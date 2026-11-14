-- Reads one mapped tree, as the plugin's first sync does, and answers one chunk of it.
--
-- INPUT: { mapping = <the mapping's description>, chunk = <0-based>, chunkBytes = <size> }
-- Answer: <<total|checksum|base64 of the chunk>>. Every call reads the tree again, so the checksum
-- tells Rust whether the chunks belong together.
local warnings = {}
TwoWayStudio.onWarn = function(message)
	table.insert(warnings, message)
end

local mapping = TwoWayStudio.newMapping(INPUT.mapping)
local current, models = TwoWayStudio.snapshot(mapping)

local entries = {}
for _, entry in current do
	table.insert(entries, entry)
end
table.sort(entries, function(a, b)
	return a.key < b.key
end)
local model_list = {}
for key, bytes in models do
	table.insert(model_list, { key = key, data = ToBase64(bytes) })
end
table.sort(model_list, function(a, b)
	return a.key < b.key
end)

local payload = HttpService:JSONEncode({
	entries = entries,
	models = model_list,
	warnings = warnings,
	found = TwoWayStudio.find(mapping, "") ~= nil,
})
local first = INPUT.chunk * INPUT.chunkBytes + 1
local piece = string.sub(payload, first, first + INPUT.chunkBytes - 1)
return "<<" .. #payload .. "|" .. Checksum(payload) .. "|" .. ToBase64(piece) .. ">>"
