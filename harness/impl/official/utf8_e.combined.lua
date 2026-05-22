-- harness preamble: emulate the globals lua-c testes/all.lua sets
_soft = true
_port = true
_nomsg = true
_U = false
arg = arg or {}
_G = _G or _ENV
if _VERSION == nil then _VERSION = "Lua 5.4" end

local utf8 = require'utf8'

print(type(table.unpack))
print(type(load))
print(type(string.byte))
print(type(string.gsub))
print(type(string.find))
print(type(string.sub))
print(type(string.gmatch))
print(type(utf8.codes))
print(type(utf8.char))
print(type(utf8.offset))
print(type(utf8.codepoint))
print(type(utf8.len))

local s = "hello World"
local t = {string.byte(s, 1, -1)}
print("#t", #t)
for i = 1, #t do io.write(t[i], " ") end
io.write("\n")

print("utf8.len(s)", utf8.len(s))
print("utf8.len(s,1,-1,nil)", utf8.len(s, 1, -1, nil))

local function len (s)
  return #string.gsub(s, "[\x80-\xBF]", "")
end
print("len(s)", len(s))

print("char unpack", utf8.char(table.unpack(t)) == s)
print("utf8.offset(s, 0)", utf8.offset(s, 0))

print "ok"
