-- harness preamble: emulate the globals lua-c testes/all.lua sets
_soft = true
_port = true
_nomsg = true
_U = false
arg = arg or {}
_G = _G or _ENV
if _VERSION == nil then _VERSION = "Lua 5.4" end

print("debug=", debug)
print("debug.traceback=", debug.traceback)
print("call:", debug.traceback("msg"))
print("call2:", debug.traceback("hello", 1))

local function foo()
  error("@x123")
end
local st, msg = xpcall(foo, debug.traceback)
print("st=", st, "msg=", msg, "type:", type(msg))
