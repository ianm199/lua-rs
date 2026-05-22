_soft = true
_port = true
_nomsg = true
_U = false
arg = arg or {}
_G = _G or _ENV
if _VERSION == nil then _VERSION = "Lua 5.4" end

print("step 1")
local n = tonumber("0xffffffffffffffff.0")
print("tonumber result =", n, type(n))
print("step 2")
local i = math.tointeger(n)
print("tointeger result =", i, type(i))
print("step 3")
local smt = getmetatable("")
print("string metatable =", smt)
print("step 4")
require "bwcoercion"
print("step 5")
local ok, err = pcall(function() return "0xffffffffffffffff.0" | 0 end)
print("ok =", ok)
print("err =", err)
