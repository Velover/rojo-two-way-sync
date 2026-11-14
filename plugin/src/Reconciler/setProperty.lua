--[[
	Attempts to set a property on the given instance.
]]

local Packages = script.Parent.Parent.Parent.Packages
local Log = require(Packages.Log)
local RbxDom = require(Packages.RbxDom)
local Error = require(script.Parent.Error)
local ScriptText = require(script.Parent.Parent.ScriptText)

local function setProperty(instance, propertyName, value)
	local descriptor = RbxDom.findCanonicalPropertyDescriptor(instance.ClassName, propertyName)

	-- We can skip unknown properties; they're not likely reflected to Lua.
	--
	-- A good example of a property like this is `Model.ModelInPrimary`, which
	-- is serialized but not reflected to Lua.
	if descriptor == nil then
		Log.trace("Skipping unknown property {}.{}", instance.ClassName, propertyName)

		return true
	end

	if descriptor.scriptability == "None" or descriptor.scriptability == "Read" then
		return false,
			Error.new(Error.UnwritableProperty, {
				className = instance.ClassName,
				propertyName = propertyName,
			})
	end

	-- The usual write replaces the whole source, which sends an open script's cursor to the end of
	-- the file. An open script is edited on only the lines that differ instead.
	if propertyName == "Source" and instance:IsA("LuaSourceContainer") and type(value) == "string" then
		local opened, isOpen = pcall(ScriptText.isOpen, instance)
		if opened and isOpen then
			local applied, err = pcall(ScriptText.apply, instance, value)
			if applied then
				return true
			end
			Log.warn("Couldn't edit {} in place, so it's rewritten in full: {}", instance:GetFullName(), err)
		end
	end

	local writeSuccess, err = descriptor:write(instance, value)

	if not writeSuccess then
		if err.kind == RbxDom.Error.Kind.Roblox and err.extra:find("lacking permission") then
			return false,
				Error.new(Error.LackingPropertyPermissions, {
					className = instance.ClassName,
					propertyName = propertyName,
				})
		end

		return false,
			Error.new(Error.OtherPropertyError, {
				className = instance.ClassName,
				propertyName = propertyName,
			})
	end

	return true
end

return setProperty
