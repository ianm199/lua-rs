-- harness preamble: emulate the globals lua-c testes/all.lua sets
_soft = true
_port = true
_nomsg = true
_U = false
arg = arg or {}
_G = _G or _ENV
if _VERSION == nil then _VERSION = "Lua 5.4" end

print("hello")
local x = debug.getinfo(1, "S")
print("short_src=", x.short_src)
print("source=", x.source)
error("boom")
