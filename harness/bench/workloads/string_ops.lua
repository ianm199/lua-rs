--[[
string_ops.lua — string concatenation, find, gsub, byte iteration.

Measures: string interning, copy-on-grow, pattern matching cost, byte-level
access. Lua's string library is byte-oriented; this exercises that.

Workload: build a 10000-character string in chunks, then run find/gsub
patterns across it 100 times.

Deterministic: outputs the number of pattern matches, which is stable.
]]

local pieces = {}
for i = 1, 1000 do
    pieces[#pieces + 1] = string.format("[item-%04d:%s]", i, "abcdefghij")
end
local big = table.concat(pieces)

local total_matches = 0
for _ = 1, 100 do
    local count = 0
    for _ in string.gmatch(big, "item%-%d+") do
        count = count + 1
    end
    total_matches = total_matches + count
end

local upper_chars = 0
local rewritten = string.gsub(big, "(%w+)", string.upper)
for i = 1, #rewritten do
    local c = string.byte(rewritten, i)
    if c >= 65 and c <= 90 then upper_chars = upper_chars + 1 end
end

assert(total_matches == 100000,
       "string_ops match count mismatch: got " .. total_matches)
assert(upper_chars == 14000,
       "string_ops upper-char count mismatch: got " .. upper_chars)
io.write("string_ops.lua OK: matches=", total_matches,
         " upper=", upper_chars, "\n")
