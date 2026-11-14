-- Carries out a batch of planned changes in one mapped tree, with the plugin's own code.
--
-- INPUT: { mapping = <description>, known = { [key] = <Studio's text when read> },
--          ops = { <op, with `blob64` for a model> } }
-- Answer: Reply({ results = { { id, ok, kept, error } }, warnings })
local warnings = {}
TwoWayStudio.onWarn = function(message)
	table.insert(warnings, message)
end

local mapping = TwoWayStudio.newMapping(INPUT.mapping)
for key, source in INPUT.known do
	mapping.known[key] = { key = key, kind = "script", source = source }
end

local results = {}
for _, op in INPUT.ops do
	local ok, done, kept = pcall(function()
		if op.action == "delete" then
			return TwoWayStudio.applyDelete(mapping, op)
		end
		local blob = if op.blob64 then FromBase64(op.blob64) else nil
		return TwoWayStudio.applyUpsert(mapping, op, blob, nil), false
	end)
	table.insert(results, {
		id = op.id,
		ok = ok and done == true,
		kept = ok and kept == true,
		error = if ok then nil else tostring(done),
	})
end

return Reply({ results = results, warnings = warnings })
