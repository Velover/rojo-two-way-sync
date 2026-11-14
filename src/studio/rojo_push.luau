-- Makes Studio's copies of a project's trees match what Rojo built, once, without a Connect.
--
-- Each item is a tree Rojo fills from files (`$path`), sent as model bytes. It is matched with
-- what Studio has by name and class: scripts and Folders are updated in place, so an open script
-- keeps its editor (its text changes through ScriptText, which keeps the cursor), and anything
-- else is replaced. Instances Rojo doesn't know are removed, unless the project keeps unknown
-- instances there. A tree too big for one call comes as a shell (the instance alone, and the names
-- of the children it keeps) and its children as items of their own.
--
-- INPUT: { dry = <only count>, items = { { path = { names }, data64, shell, children,
--          keepUnknown = { <relative paths> } } } }
-- Answer: Reply({ created, updated, replaced, removed, unchanged, errors })
local ScriptEditorService = game:GetService("ScriptEditorService")
local SerializationService = game:GetService("SerializationService")

local dry = INPUT.dry == true
local stats = {
	created = 0,
	updated = 0,
	replaced = 0,
	removed = 0,
	unchanged = 0,
	errors = {},
	-- The first paths of each kind, for the report.
	paths = { created = {}, updated = {}, replaced = {}, removed = {} },
}
local MAX_PATHS = 50

local function PathOf(instance)
	local names = {}
	while instance and instance ~= game do
		table.insert(names, 1, instance.Name)
		instance = instance.Parent
	end
	return table.concat(names, "/")
end

local function Count(kind, path)
	stats[kind] += 1
	if #stats.paths[kind] < MAX_PATHS then
		table.insert(stats.paths[kind], path)
	end
end

local function IsContainer(instance)
	return instance:IsA("Folder") or instance:IsA("LuaSourceContainer")
end

local function SameMeta(a, b)
	for name, value in a:GetAttributes() do
		if b:GetAttribute(name) ~= value then
			return false
		end
	end
	for name in b:GetAttributes() do
		if a:GetAttribute(name) == nil then
			return false
		end
	end
	local tags_a, tags_b = a:GetTags(), b:GetTags()
	table.sort(tags_a)
	table.sort(tags_b)
	return table.concat(tags_a, "\0") == table.concat(tags_b, "\0")
end

local function CopyMeta(from, to)
	for name in to:GetAttributes() do
		if from:GetAttribute(name) == nil then
			to:SetAttribute(name, nil)
		end
	end
	for name, value in from:GetAttributes() do
		if to:GetAttribute(name) ~= value then
			to:SetAttribute(name, value)
		end
	end
	for _, tag in to:GetTags() do
		if not from:HasTag(tag) then
			to:RemoveTag(tag)
		end
	end
	for _, tag in from:GetTags() do
		to:AddTag(tag)
	end
end

local function Replace(existing, new)
	local path = PathOf(existing)
	if not dry then
		new.Name = existing.Name
		new.Parent = existing.Parent
		existing:Destroy()
	end
	Count("replaced", path)
	return new
end

-- Studio serializes the same instances to the same bytes, so this tells whether two differ.
local function Serialized(instance)
	local ok, bytes = pcall(function()
		return buffer.tostring(SerializationService:SerializeInstancesAsync({ instance }))
	end)
	return if ok then bytes else nil
end

-- Updates `existing` to match `new` (from the model bytes). `relative` is its path inside the item,
-- for `keepUnknown`; `shell_children` lists the children a shell keeps, which arrive on their own.
local function Reconcile(existing, new, relative, keep_unknown, shell_children)
	if existing.ClassName ~= new.ClassName or not IsContainer(existing) then
		if existing.ClassName == new.ClassName and not shell_children then
			local before = Serialized(existing)
			if before ~= nil and before == Serialized(new) then
				stats.unchanged += 1
				return
			end
		end
		if shell_children then
			-- A shell's children come separately: keep them under the new instance.
			if not dry then
				for _, child in existing:GetChildren() do
					child.Parent = new
				end
			end
		end
		Replace(existing, new)
		return
	end

	local changed = false
	if existing:IsA("LuaSourceContainer") then
		local text = new.Source
		if ScriptEditorService:GetEditorSource(existing) ~= text then
			changed = true
			if not dry then
				ScriptText.apply(existing, text)
			end
		end
		if existing:IsA("Script") and existing.RunContext ~= new.RunContext then
			changed = true
			if not dry then
				existing.RunContext = new.RunContext
			end
		end
	end
	if not SameMeta(new, existing) then
		changed = true
		if not dry then
			CopyMeta(new, existing)
		end
	end
	if changed then
		Count("updated", PathOf(existing))
	else
		stats.unchanged += 1
	end

	local keeps_unknown = keep_unknown[relative] == true
	if shell_children then
		local wanted = {}
		for _, name in shell_children do
			wanted[name] = true
		end
		for _, child in existing:GetChildren() do
			if not wanted[child.Name] and not keeps_unknown then
				Count("removed", PathOf(child))
				if not dry then
					child:Destroy()
				end
			end
		end
		return
	end

	local by_name = {}
	for _, child in existing:GetChildren() do
		by_name[child.Name] = by_name[child.Name] or {}
		table.insert(by_name[child.Name], child)
	end
	for _, child in new:GetChildren() do
		local list = by_name[child.Name]
		local match = nil
		if list then
			for index, candidate in list do
				if candidate.ClassName == child.ClassName then
					match = table.remove(list, index)
					break
				end
			end
			match = match or table.remove(list, 1)
		end
		local child_relative = if relative == "" then child.Name else relative .. "/" .. child.Name
		if match then
			Reconcile(match, child, child_relative, keep_unknown, nil)
		else
			Count("created", PathOf(existing) .. "/" .. child.Name)
			if not dry then
				child.Parent = existing
			end
		end
	end
	if not keeps_unknown then
		for _, list in by_name do
			for _, left in list do
				Count("removed", PathOf(left))
				if not dry then
					left:Destroy()
				end
			end
		end
	end
end

-- The instance at a path from the DataModel, making missing Folders (and services) when asked.
local function Resolve(path, make)
	local ok, current = pcall(function()
		return game:GetService(path[1])
	end)
	if not ok or not current then
		current = game:FindFirstChild(path[1])
	end
	if not current then
		return nil
	end
	for i = 2, #path do
		local child = current:FindFirstChild(path[i])
		if not child then
			if not make then
				return nil
			end
			child = Instance.new("Folder")
			child.Name = path[i]
			if not dry then
				child.Parent = current
			end
		end
		current = child
	end
	return current
end

for _, item in INPUT.items do
	local ok, problem = pcall(function()
		local created = SerializationService:DeserializeInstancesAsync(buffer.fromstring(FromBase64(item.data64)))
		local new = created[1]
		local keep_unknown = {}
		for _, relative in item.keepUnknown or {} do
			keep_unknown[relative] = true
		end
		local parent_path = table.clone(item.path)
		local name = table.remove(parent_path)
		local parent = Resolve(parent_path, true)
		if not parent then
			error("no " .. table.concat(parent_path, "/") .. " to put it in")
		end
		local existing = parent:FindFirstChild(name)
		if existing then
			Reconcile(existing, new, "", keep_unknown, if item.shell then item.children else nil)
		else
			Count("created", table.concat(item.path, "/"))
			if not dry then
				new.Name = name
				new.Parent = parent
			end
		end
	end)
	if not ok then
		table.insert(stats.errors, table.concat(item.path, "/") .. ": " .. tostring(problem))
	end
end

-- For the properties and attributes project nodes set, generated below: each compares first.
local function Differs(a, b)
	if type(a) == "number" and type(b) == "number" then
		-- Studio keeps most numbers at 32 bits.
		return math.abs(a - b) > 1e-6 * math.max(1, math.abs(b))
	end
	return a ~= b
end

local function SetProperty(target, name, value)
	if not Differs(target[name], value) then
		return false
	end
	if not dry then
		target[name] = value
	end
	return true
end

local function SetAttribute(target, name, value)
	if not Differs(target:GetAttribute(name), value) then
		return false
	end
	if not dry then
		target:SetAttribute(name, value)
	end
	return true
end

--[[PROPERTIES]]

return Reply(stats)
