--[[
	The Studio side of the two-way sync: reading a mapped tree (scripts, folders, attributes and
	tags, and model bytes) and carrying out one change. No networking and no session, so the same
	code serves the plugin's live sync (TwoWay.lua) and `rojo studio push`, which runs it in Studio
	over MCP with the module's text inlined.

	A mapping is the table `TwoWayStudio.newMapping` makes from the server's description of it.
	`mapping.known` holds what was last reported for each key, so a change never overwrites text
	typed since.
]]

local ScriptEditorService = game:GetService("ScriptEditorService")
local SerializationService = game:GetService("SerializationService")

local ScriptText = require(script.Parent.ScriptText)

local TwoWayStudio = {}

-- Set by the caller to hear about problems worth showing; each message once.
TwoWayStudio.onWarn = nil

local function Warn(message)
	if TwoWayStudio.onWarn then
		TwoWayStudio.onWarn(message)
	end
end

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
			Warn(instance:GetFullName() .. "." .. name .. " is a " .. typeof(value) .. ", which can't be saved")
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
				Warn("couldn't set " .. instance:GetFullName() .. "." .. name .. ": " .. tostring(problem))
			end
		else
			Warn("couldn't read the value of " .. instance:GetFullName() .. "." .. name)
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
				Warn(child:GetFullName() .. " shares its name with a sibling and isn't synced")
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

local function SerializeList(list, label)
	local ok, bytes = pcall(function()
		return buffer.tostring(SerializationService:SerializeInstancesAsync(list))
	end)
	if ok then
		return bytes
	end
	Warn("could not save " .. label .. ": " .. tostring(bytes))
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

local function ApplyScriptSource(instance, text)
	if not ScriptText.apply(instance, text) then
		Warn("had to rewrite " .. instance:GetFullName() .. " in full")
	end
end

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

local function ApplyUpsert(mapping, op, blob, onTyped)
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
		local created = SerializationService:DeserializeInstancesAsync(buffer.fromstring(blob))
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
		local created = SerializationService:DeserializeInstancesAsync(buffer.fromstring(blob))
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
	-- The server speaks Script Sync's names too: they're the same families.
	if op.scriptType == "local" or op.scriptType == "legacy" then
		same_family = instance and ScriptTypeOf(instance) == (op.scriptType == "local" and "client" or "server")
	end

	if same_family then
		local editor = ScriptEditorService:GetEditorSource(instance)
		local previous = mapping.known[op.key]
		if editor ~= op.source then
			if previous and previous.source ~= editor then
				-- Typed since the last report: don't overwrite it. The caller reports it, and the
				-- server sees both edits and asks.
				if onTyped then
					onTyped(mapping)
				end
				return false
			end
			ApplyScriptSource(instance, op.source)
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
		-- Studio refuses a long Source set directly from some contexts (MCP's execute_luau);
		-- UpdateSourceAsync takes any length once the script is in the place.
		local set_directly = pcall(function()
			script.Source = op.source
		end)
		ApplyMeta(script, op.attributes, op.tags)
		if instance then
			Replace(instance, script)
		else
			script.Parent = EnsureParent(mapping, op.key)
		end
		if not set_directly then
			ScriptEditorService:UpdateSourceAsync(script, function()
				return op.source
			end)
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

-- A mapping, from the server's description: `index`, `studio`, `models`, `auto`, `exclude`,
-- `group` and `separate`.
function TwoWayStudio.newMapping(info)
	local set, patterns = CompileList(info.models)
	local exclude_set, exclude_patterns = CompileList(info.exclude)
	local separate_set, separate_patterns = CompileList(info.separate)
	return {
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
	}
end

-- Everything under the mapping's root, by key.
TwoWayStudio.scan = ScanMapping

-- What a first sync sends: every entry, and the bytes of each model by key. Models Studio can't
-- save are left out of both.
function TwoWayStudio.snapshot(mapping)
	local current = ScanMapping(mapping)
	local models = {}
	for _, key in mapping.modelKeys do
		local bytes = SerializeKey(mapping, key)
		if bytes then
			models[key] = bytes
		elseif not IsModel(mapping, key) then
			mapping.unsavable[key] = true
		end
	end
	for key in mapping.unsavable do
		current[key] = nil
		models[key] = nil
	end
	return current, models
end

TwoWayStudio.same = Same
TwoWayStudio.serializeKey = SerializeKey
TwoWayStudio.isModel = IsModel
TwoWayStudio.isGroupKey = IsGroupKey
TwoWayStudio.parentKey = ParentKey
TwoWayStudio.find = Find

-- Carries out an upsert. `blob` is the model's bytes for a model. Returns false when Studio's text
-- changed since it was last reported (`onTyped` hears of it) and nothing was written.
TwoWayStudio.applyUpsert = ApplyUpsert

-- Carries out a delete. Returns `done, kept`: kept means the instance holds things that aren't
-- synced, so it stays.
TwoWayStudio.applyDelete = ApplyDelete

return TwoWayStudio
