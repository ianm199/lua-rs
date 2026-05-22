_soft = true
_port = true
_nomsg = true
_U = false
arg = arg or {}
_G = _G or _ENV
if _VERSION == nil then _VERSION = "Lua 5.4" end

print('testing strings and string library')

local maxi <const> = math.maxinteger
local mini <const> = math.mininteger


local function checkerror (msg, f, ...)
  local s, err = pcall(f, ...)
  assert(not s and string.find(err, msg))
end


-- testing string comparisons
assert('alo' < 'alo1')

-- testing string.sub
assert(string.sub("123456789",2,4) == "234")

-- testing string.find
assert(string.find("123456789", "345") == 3)
local a,b = string.find("123456789", "345")
assert(string.sub("123456789", a, b) == "345")
print("got past find")
assert(string.find("1234567890123456789", "345", 3) == 3)
assert(string.find("1234567890123456789", "345", 4) == 13)
assert(not string.find("1234567890123456789", "346", 4))
assert(string.find("1234567890123456789", ".45", -9) == 13)
print("find batch1 done")
assert(not string.find("abcdefg", "\0", 5, 1))
print("find with 0 done")
assert(string.find("", "") == 1)
assert(string.find("", "", 1) == 1)
assert(not string.find("", "", 2))
assert(not string.find('', 'aaa', 1))
print("string.find done")

assert(string.len("") == 0)
print("len done")

assert(string.byte("a") == 97)
print("byte done")

assert(string.char() == "")
assert(string.char(0, 255, 0) == "\0\255\0")
print("char done")

checkerror("out of range", string.char, 256)
print("checkerror char done")

assert(string.upper("ab\0c") == "AB\0C")
print("upper done")
assert(string.lower("\0ABCc%$") == "\0abcc%$")
assert(string.rep('teste', 0) == '')
print("rep basic done")

assert(string.reverse"" == "")
print("reverse done")

assert(type(tostring(nil)) == 'string')
assert(type(tostring(12)) == 'string')
assert(string.find(tostring{}, 'table:'))
print("tostring done")

print("part1 done")
