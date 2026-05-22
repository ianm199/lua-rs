-- harness preamble: emulate the globals lua-c testes/all.lua sets
_soft = true
_port = true
_nomsg = true
_U = false
arg = arg or {}
_G = _G or _ENV
if _VERSION == nil then _VERSION = "Lua 5.4" end

local utf8 = require'utf8'

local s = "hello World"
local t = {string.byte(s, 1, -1)}

print("utf8.charpattern type:", type(utf8.charpattern))
print("utf8.charpattern:", utf8.charpattern)
print("string.gmatch type:", type(string.gmatch))

local iter = string.gmatch(s, utf8.charpattern)
print("iter type:", type(iter))

local i = 0
for c in iter do
  i = i + 1
  print("got", i, c, "t[i]=", t[i])
end
print("loop done, i=", i)
