-- harness preamble: emulate the globals lua-c testes/all.lua sets
_soft = true
_port = true
_nomsg = true
_U = false
arg = arg or {}
_G = _G or _ENV
if _VERSION == nil then _VERSION = "Lua 5.4" end

print "testing closures"

local A,B = 0,{g=10}
local function f(x)
  local a = {}
  for i=1,3 do
    local y = 0
    do
      a[i] = function () B.g = B.g+1; y = y+x; return y+A end
    end
  end
  local dummy = function () return a[A] end
  collectgarbage()
  A = 1; assert(dummy() == a[1]); A = 0;
  assert(a[1]() == x)
  assert(a[3]() == x)
  collectgarbage()
  return a
end

local a = f(10)
print("ok")
