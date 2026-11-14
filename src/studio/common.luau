-- Shared by every snippet `rojo studio` runs in Studio over MCP. Rust puts the input, as JSON, in
-- place of the INPUT marker below. Answers go back as base64 between << and >>, so any byte
-- survives the trip and the proxy's quoting.
local HttpService = game:GetService("HttpService")
local EncodingService = game:GetService("EncodingService")

local function FromBase64(text: string): string
	return buffer.tostring(EncodingService:Base64Decode(buffer.fromstring(text)))
end

local function ToBase64(text: string): string
	return buffer.tostring(EncodingService:Base64Encode(buffer.fromstring(text)))
end

local function Reply(value): string
	return "<<" .. ToBase64(HttpService:JSONEncode(value)) .. ">>"
end

-- FNV-1a, 32 bits: tells whether two reads of a long answer were the same text.
local function Checksum(text: string): number
	local bytes = buffer.fromstring(text)
	local hash = 2166136261
	for i = 0, buffer.len(bytes) - 1 do
		hash = bit32.bxor(hash, buffer.readu8(bytes, i))
		-- hash * 16777619, kept exact: the prime is 2^24 + 403.
		hash = (hash * 403 + bit32.lshift(hash, 24)) % 4294967296
	end
	return hash
end

local INPUT = HttpService:JSONDecode(--[[INPUT]])
