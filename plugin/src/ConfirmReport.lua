--[[
	Describes a patch in plain data for the server's confirm page: each instance it adds, removes
	or changes, by path, with the values of the properties that change.

	This replaces building a PatchTree in Studio before the user can accept the first sync. Values
	are turned into text here because the page can't read Roblox types.
]]

local Rojo = script:FindFirstAncestor("Rojo")
local Plugin = Rojo.Plugin

local decodeValue = require(Plugin.Reconciler.decodeValue)
local getProperty = require(Plugin.Reconciler.getProperty)

-- Past this many added instances, only their paths are listed: a first sync into an empty place
-- adds the whole project, and its properties would make the report huge.
local MAX_DETAILED_ADDS = 500

-- Long values are cut, so one odd property can't bloat the report.
local MAX_VALUE_LENGTH = 400000

local ConfirmReport = {}

local function describe(value: any): any
	local kind = typeof(value)
	if kind == "string" then
		if #value > MAX_VALUE_LENGTH then
			return string.sub(value, 1, MAX_VALUE_LENGTH) .. "\n... (cut)"
		end
		return value
	elseif kind == "number" or kind == "boolean" then
		return value
	elseif kind == "nil" then
		return "nil"
	elseif kind == "Instance" then
		return value:GetFullName()
	elseif kind == "table" then
		local parts = {}
		for key, item in value do
			table.insert(parts, tostring(key) .. " = " .. tostring(describe(item)))
		end
		table.sort(parts)
		return "{ " .. table.concat(parts, ", ") .. " }"
	end
	return tostring(value)
end

-- Instance paths with "/" between names, like the rest of Rojo's tooling.
local function pathOf(instance: Instance): string
	if instance == game then
		return "game"
	end
	local names = {}
	local current = instance
	while current and current ~= game do
		table.insert(names, 1, current.Name)
		current = current.Parent
	end
	return table.concat(names, "/")
end

local function pause(clock)
	if os.clock() - clock > 1 / 30 then
		task.wait()
		return os.clock()
	end
	return clock
end

function ConfirmReport.build(patch, instanceMap, projectName: string)
	local items = {}
	local counts = { added = 0, removed = 0, changed = 0 }
	local clock = os.clock()

	for _, change in patch.updated do
		clock = pause(clock)
		local instance = instanceMap.fromIds[change.id]
		if not instance then
			continue
		end
		counts.changed += 1

		local changes = {}
		if change.changedName then
			table.insert(changes, { name = "Name", current = instance.Name, incoming = change.changedName })
		end
		if change.changedClassName then
			table.insert(
				changes,
				{ name = "ClassName", current = instance.ClassName, incoming = change.changedClassName }
			)
		end
		for prop, incoming in change.changedProperties do
			local incomingOk, incomingValue = decodeValue(incoming, instanceMap)
			local currentOk, currentValue = getProperty(instance, prop)
			table.insert(changes, {
				name = prop,
				current = if currentOk then describe(currentValue) else "[couldn't read]",
				incoming = describe(if incomingOk then incomingValue else select(2, next(incoming))),
			})
		end
		table.sort(changes, function(a, b)
			return a.name < b.name
		end)

		table.insert(items, {
			kind = "Edit",
			path = pathOf(instance),
			className = instance.ClassName,
			changes = changes,
		})
	end

	for _, idOrInstance in patch.removed do
		clock = pause(clock)
		local instance = if typeof(idOrInstance) == "Instance" then idOrInstance else instanceMap.fromIds[idOrInstance]
		if not instance then
			continue
		end
		counts.removed += 1
		table.insert(items, {
			kind = "Remove",
			path = pathOf(instance),
			className = instance.ClassName,
		})
	end

	local addCount = 0
	for _ in patch.added do
		addCount += 1
	end
	local detailed = addCount <= MAX_DETAILED_ADDS

	-- An added instance's parent is either already in Studio or added by the same patch.
	local addedPaths = {}
	local function addedPath(id): string
		if addedPaths[id] then
			return addedPaths[id]
		end
		local added = patch.added[id]
		if not added then
			local existing = instanceMap.fromIds[id]
			return if existing then pathOf(existing) else "?"
		end
		local parentPath = if added.Parent then addedPath(added.Parent) else ""
		local path = if parentPath == "" then added.Name else parentPath .. "/" .. added.Name
		addedPaths[id] = path
		return path
	end

	for id, added in patch.added do
		clock = pause(clock)
		counts.added += 1
		local changes = {}
		if detailed then
			for prop, incoming in added.Properties do
				local ok, value = decodeValue(incoming, instanceMap)
				table.insert(changes, {
					name = prop,
					incoming = describe(if ok then value else select(2, next(incoming))),
				})
			end
			table.sort(changes, function(a, b)
				return a.name < b.name
			end)
		end
		table.insert(items, {
			kind = "Add",
			path = addedPath(id),
			className = added.ClassName,
			changes = changes,
		})
	end

	table.sort(items, function(a, b)
		return a.path < b.path
	end)

	return {
		projectName = projectName,
		counts = counts,
		items = items,
		truncated = not detailed,
	}
end

return ConfirmReport
