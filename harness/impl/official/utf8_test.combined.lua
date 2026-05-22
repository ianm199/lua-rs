-- harness preamble: emulate the globals lua-c testes/all.lua sets
_soft = true
_port = true
_nomsg = true
_U = false
arg = arg or {}
_G = _G or _ENV
if _VERSION == nil then _VERSION = "Lua 5.4" end

print("test: gsub with no match")
local x, n = string.gsub("hello", "[\x80-\xBF]", "")
print("x type:", type(x), "val:", x)
print("n type:", type(n), "val:", n)

print("test: gsub with no match, single var")
local y = string.gsub("hello", "[\x80-\xBF]", "")
print("y type:", type(y), "val:", y)

print("test: gsub direct in len")
local r = #string.gsub("hello", "[\x80-\xBF]", "")
print("r =", r)
