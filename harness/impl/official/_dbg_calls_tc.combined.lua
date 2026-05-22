-- harness preamble: emulate the globals lua-c testes/all.lua sets
_soft = true
_port = true
_nomsg = true
_U = false
arg = arg or {}
_G = _G or _ENV
if _VERSION == nil then _VERSION = "Lua 5.4" end

do   -- C-stack overflow while handling C-stack overflow
  local function loop ()
    assert(pcall(loop))
  end

  local err, msg = xpcall(loop, loop)
  print("err=", err, "msg=", msg, "type(msg)=", type(msg))
end
