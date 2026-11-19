--[[
	Properties Roblox links together, so a value Rojo sets is not always the value Studio keeps.

	Turning TextScaled on also turns TextWrapped on, and turning TextWrapped off also turns
	TextScaled off. A model saved with TextScaled on and TextWrapped off (older files have it)
	therefore can't be reproduced through properties: comparing TextWrapped reports a change on
	every connect, and setting it to false turns the text's scaling off. Scaled text wraps
	anyway, so while TextScaled is on, TextWrapped is left alone.
]]

local linkedProperties = {}

local function isTrue(encodedValue): boolean
	return type(encodedValue) == "table" and encodedValue.Bool == true
end

-- Whether a property should be neither compared nor set, given the encoded properties the
-- instance should end up with.
function linkedProperties.skip(propertyName: string, encodedProperties): boolean
	return propertyName == "TextWrapped" and isTrue(encodedProperties.TextScaled)
end

-- The same for an update, which carries only the properties that changed: TextScaled comes from
-- the update when it changes and from the instance otherwise.
function linkedProperties.skipUpdate(propertyName: string, changedProperties, instance: Instance): boolean
	if propertyName ~= "TextWrapped" then
		return false
	end
	local scaled = changedProperties.TextScaled
	if scaled ~= nil then
		return isTrue(scaled)
	end
	local ok, current = pcall(function()
		return (instance :: any).TextScaled
	end)
	return ok and current == true
end

return linkedProperties
