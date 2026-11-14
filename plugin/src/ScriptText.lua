--[[
	Changes a script's source without moving the cursor of whoever has it open.

	ScriptEditorService:UpdateSourceAsync replaces the whole text, which sends an open script's
	cursor to the end of the file. ScriptDocument:EditTextAsync on only the lines that differ
	leaves it where it was. Used for Rojo's own patches and for the two-way sync.
]]

local ScriptEditorService = game:GetService("ScriptEditorService")

-- Past this many cells the line diff falls back to one block, to keep big rewrites quick.
local MAX_DIFF_CELLS = 2000000

local ScriptText = {}

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

-- Sets a script's source. An open script is edited on only the lines that differ, so its cursor,
-- selection and scroll stay where they are. Returns false when it had to fall back to a whole
-- rewrite to get the text right.
function ScriptText.apply(instance: LuaSourceContainer, text: string): boolean
	local document = ScriptEditorService:FindScriptDocument(instance)
	if not document then
		WholeWrite(instance, text)
		return true
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
		return false
	end
	return true
end

-- True when the script is open in an editor tab.
function ScriptText.isOpen(instance: LuaSourceContainer): boolean
	return ScriptEditorService:FindScriptDocument(instance) ~= nil
end

return ScriptText
