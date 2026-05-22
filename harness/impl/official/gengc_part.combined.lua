-- harness preamble: emulate the globals lua-c testes/all.lua sets
_soft = true
_port = true
_nomsg = true
_U = false
arg = arg or {}
_G = _G or _ENV
if _VERSION == nil then _VERSION = "Lua 5.4" end

local co = coroutine.create(
  function ()
    local x = nil
    local f = function ()
                return x[1]
              end
    print("about to yield f, type=", type(f))
    x = coroutine.yield(f)
    print("resumed, x=", x)
    coroutine.yield()
  end
)
print("created co, type=", type(co))
local ok, f = coroutine.resume(co)
print("resume returned: ok=", ok, "f=", f, "type=", type(f))
