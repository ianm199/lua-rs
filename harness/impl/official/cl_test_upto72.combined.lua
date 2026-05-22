-- harness preamble: emulate the globals lua-c testes/all.lua sets
_soft = true
_port = true
_nomsg = true
_U = false
arg = arg or {}
_G = _G or _ENV
if _VERSION == nil then _VERSION = "Lua 5.4" end

-- $Id: testes/closure.lua $

print "testing closures"

local A,B = 0,{g=10}
local function f(x)
  local a = {}
  for i=1,1000 do
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
  assert(B.g == 12)
  return a
end

local a = f(10)
local x = {[1] = {}}
setmetatable(x, {__mode = 'kv'})
while x[1] do
  local a = A..A..A..A
  A = A+1
end
assert(a[1]() == 20+A)
assert(a[1]() == 30+A)
assert(a[2]() == 10+A)
collectgarbage()
assert(a[2]() == 20+A)
assert(a[2]() == 30+A)
assert(a[3]() == 20+A)
assert(a[8]() == 10+A)
assert(getmetatable(x).__mode == 'kv')
assert(B.g == 19)


-- testing equality
a = {}

for i = 1, 5 do  a[i] = function (x) return i + a + _ENV end  end
assert(a[3] ~= a[4] and a[4] ~= a[5])

do
  local a = function (x)  return math.sin(_ENV[x])  end
  local function f()
    return a
  end
  assert(f() == f())
end


-- testing closures with 'for' control variable
a = {}
for i=1,10 do
  a[i] = {set = function(x) i=x end, get = function () return i end}
  if i == 3 then break end
end
assert(a[4] == undef)
a[1].set(10)
assert(a[2].get() == 2)
a[2].set('a')
assert(a[3].get() == 3)
assert(a[2].get() == 'a')

print "OK to 72"
