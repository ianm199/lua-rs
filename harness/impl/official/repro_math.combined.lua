-- harness preamble: emulate the globals lua-c testes/all.lua sets
_soft = true
_port = true
_nomsg = true
_U = false
arg = arg or {}
_G = _G or _ENV
if _VERSION == nil then _VERSION = "Lua 5.4" end

_soft = true
_port = true
_nomsg = true
_U = false
arg = arg or {}
_G = _G or _ENV
if _VERSION == nil then _VERSION = "Lua 5.4" end

print("testing numbers and math lib")

local minint <const> = math.mininteger
local maxint <const> = math.maxinteger

local intbits <const> = math.floor(math.log(maxint, 2) + 0.5) + 1
assert((1 << intbits) == 0)

assert(minint == 1 << (intbits - 1))
assert(maxint == minint - 1)

-- number of bits in the mantissa of a floating-point number
local floatbits = 24
do
  local p = 2.0^floatbits
  while p < p + 1.0 do
    p = p * 2.0
    floatbits = floatbits + 1
  end
end

local function isNaN (x)
  return (x ~= x)
end

assert(isNaN(0/0))
assert(not isNaN(1/0))


do
  local x = 2.0^floatbits
  assert(x > x - 1.0 and x == x + 1.0)

  print(string.format("%d-bit integers, %d-bit (mantissa) floats",
                       intbits, floatbits))
end

assert(math.type(0) == "integer" and math.type(0.0) == "float"
       and not math.type("10"))


local function checkerror (msg, f, ...)
  local s, err = pcall(f, ...)
  assert(not s and string.find(err, msg))
end

local msgf2i = "number.* has no integer representation"

-- float equality
local function eq (a,b,limit)
  if not limit then
    if floatbits >= 50 then limit = 1E-11
    else limit = 1E-5
    end
  end
  -- a == b needed for +inf/-inf
  return a == b or math.abs(a-b) <= limit
end


-- equality with types
local function eqT (a,b)
  return a == b and math.type(a) == math.type(b)
end


-- basic float notation
assert(0e12 == 0 and .0 == 0 and 0. == 0 and .2e2 == 20 and 2.E-1 == 0.2)

do
  local a,b,c = "2", " 3e0 ", " 10  "
  assert(a+b == 5 and -b == -3 and b+"2" == 5 and "10"-c == 0)
  assert(type(a) == 'string' and type(b) == 'string' and type(c) == 'string')
  assert(a == "2" and b == " 3e0 " and c == " 10  " and -c == -"  10 ")
  assert(c%a == 0 and a^b == 08)
  a = 0
  assert(a == -a and 0 == -0)
end

do
  local x = -1
  local mz = 0/x   -- minus zero
  local t = {[0] = 10, 20, 30, 40, 50}
  assert(t[mz] == t[0] and t[-0] == t[0])
end

do   -- tests for 'modf'
  local a,b = math.modf(3.5)
  assert(a == 3.0 and b == 0.5)
  a,b = math.modf(-2.5)
  assert(a == -2.0 and b == -0.5)
  a,b = math.modf(-3e23)
  assert(a == -3e23 and b == 0.0)
  a,b = math.modf(3e35)
  assert(a == 3e35 and b == 0.0)
  a,b = math.modf(-1/0)   -- -inf
  assert(a == -1/0 and b == 0.0)
  a,b = math.modf(1/0)   -- inf
  assert(a == 1/0 and b == 0.0)
  a,b = math.modf(0/0)   -- NaN
  assert(isNaN(a) and isNaN(b))
  a,b = math.modf(3)  -- integer argument
  assert(eqT(a, 3) and eqT(b, 0.0))
  a,b = math.modf(minint)
  assert(eqT(a, minint) and eqT(b, 0.0))
end

assert(math.huge > 10e30)
assert(-math.huge < -10e30)


-- integer arithmetic
assert(minint < minint + 1)
assert(maxint - 1 < maxint)
assert(0 - minint == minint)
assert(minint * minint == 0)
assert(maxint * maxint * maxint == maxint)


-- testing floor division and conversions

for _, i in pairs{-16, -15, -3, -2, -1, 0, 1, 2, 3, 15} do
  for _, j in pairs{-16, -15, -3, -2, -1, 1, 2, 3, 15} do
    for _, ti in pairs{0, 0.0} do     -- try 'i' as integer and as float
      for _, tj in pairs{0, 0.0} do   -- try 'j' as integer and as float
        local x = i + ti
        local y = j + tj
          assert(i//j == math.floor(i/j))
      end
    end
  end
end

assert(1//0.0 == 1/0)
assert(-1 // 0.0 == -1/0)
assert(eqT(3.5 // 1.5, 2.0))
assert(eqT(3.5 // -1.5, -3.0))

do   -- tests for different kinds of opcodes
  local x, y
  x = 1; assert(x // 0.0 == 1/0)
  x = 1.0; assert(x // 0 == 1/0)
  x = 3.5; assert(eqT(x // 1, 3.0))
  assert(eqT(x // -1, -4.0))

  x = 3.5; y = 1.5; assert(eqT(x // y, 2.0))
  x = 3.5; y = -1.5; assert(eqT(x // y, -3.0))
end

assert(maxint // maxint == 1)
assert(maxint // 1 == maxint)
assert((maxint - 1) // maxint == 0)
assert(maxint // (maxint - 1) == 1)
assert(minint // minint == 1)
assert(minint // minint == 1)
assert((minint + 1) // minint == 0)
assert(minint // (minint + 1) == 1)
assert(minint // 1 == minint)

assert(minint // -1 == -minint)
assert(minint // -2 == 2^(intbits - 2))
assert(maxint // -1 == -maxint)


-- negative exponents
do
  assert(2^-3 == 1 / 2^3)
  assert(eq((-3)^-3, 1 / (-3)^3))
  for i = -3, 3 do    -- variables avoid constant folding
      for j = -3, 3 do
        -- domain errors (0^(-n)) are not portable
        if not _port or i ~= 0 or j > 0 then
          assert(eq(i^j, 1 / i^(-j)))
       end
    end
  end
end

-- comparison between floats and integers (border cases)
if floatbits < intbits then
  assert(2.0^floatbits == (1 << floatbits))
  assert(2.0^floatbits - 1.0 == (1 << floatbits) - 1.0)
  assert(2.0^floatbits - 1.0 ~= (1 << floatbits))
  -- float is rounded, int is not
  assert(2.0^floatbits + 1.0 ~= (1 << floatbits) + 1)
else   -- floats can express all integers with full accuracy
  assert(maxint == maxint + 0.0)
  assert(maxint - 1 == maxint - 1.0)
  assert(minint + 1 == minint + 1.0)
  assert(maxint ~= maxint - 1.0)
end
assert(maxint + 0.0 == 2.0^(intbits - 1) - 1.0)
assert(minint + 0.0 == minint)
assert(minint + 0.0 == -2.0^(intbits - 1))


-- order between floats and integers
assert(1 < 1.1); assert(not (1 < 0.9))
assert(1 <= 1.1); assert(not (1 <= 0.9))
assert(-1 < -0.9); assert(not (-1 < -1.1))
assert(1 <= 1.1); assert(not (-1 <= -1.1))
assert(-1 < -0.9); assert(not (-1 < -1.1))
assert(-1 <= -0.9); assert(not (-1 <= -1.1))
assert(minint <= minint + 0.0)
assert(minint + 0.0 <= minint)
assert(not (minint < minint + 0.0))
assert(not (minint + 0.0 < minint))
assert(maxint < minint * -1.0)
assert(maxint <= minint * -1.0)

do
  local fmaxi1 = 2^(intbits - 1)
  assert(maxint < fmaxi1)
  assert(maxint <= fmaxi1)
  assert(not (fmaxi1 <= maxint))
  assert(minint <= -2^(intbits - 1))
  assert(-2^(intbits - 1) <= minint)
end

if floatbits < intbits then
  print("testing order (floats cannot represent all integers)")
  local fmax = 2^floatbits
  print("fmax=", fmax)
  local ifmax = fmax | 0
  print("ifmax=", ifmax)
  assert(fmax < ifmax + 1)
  print("ok")
end
