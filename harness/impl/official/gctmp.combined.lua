-- harness preamble: emulate the globals lua-c testes/all.lua sets
_soft = true
_port = true
_nomsg = true
_U = false
arg = arg or {}
_G = _G or _ENV
if _VERSION == nil then _VERSION = "Lua 5.4" end

collectgarbage"stop"
print("c0=", collectgarbage("count"))
collectgarbage()
print("c1=", collectgarbage("count"))
print("done")
