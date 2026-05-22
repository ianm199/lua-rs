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
  print "P1"
  local dummy = function () return a[A] end
  print "P2"
  collectgarbage()
  print "P3"
  A = 1; assert(dummy() == a[1]); A = 0;
  print "P4"
  assert(a[1]() == x)
  print "P5"
  assert(a[3]() == x)
  print "P6"
  collectgarbage()
  print "P7"
  return a
end

local a = f(10)
print("ok")
