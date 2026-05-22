-- harness preamble: emulate the globals lua-c testes/all.lua sets
_soft = true
_port = true
_nomsg = true
_U = false
arg = arg or {}
_G = _G or _ENV
if _VERSION == nil then _VERSION = "Lua 5.4" end

local function func2close (f)
  return setmetatable({}, {__close = f})
end

local function foo (...)
  local x123 <close> = func2close(function () error("@x123") end)
end

local st, msg = xpcall(foo, debug.traceback)
print("st=", st)
print("msg=", msg)
print("match:", string.match(msg, "^[^ ]* @x123"))
print("find close:", string.find(msg, "in metamethod 'close'"))
