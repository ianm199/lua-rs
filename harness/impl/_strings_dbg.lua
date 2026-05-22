-- harness preamble
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
assert('alo' < 'alo1'); print("L20 OK")
assert('' < 'a'); print("L21 OK")
assert('alo\0alo' < 'alo\0b')
assert('alo\0alo\0\0' > 'alo\0alo\0')
assert('alo' < 'alo\0')
assert('alo\0' > 'alo')
assert('\0' < '\1')
assert('\0\0' < '\0\1')
assert('\1\0a\0a' <= '\1\0a\0a')
assert(not ('\1\0a\0b' <= '\1\0a\0a'))
assert('\0\0\0' < '\0\0\0\0')
assert(not('\0\0\0\0' < '\0\0\0'))
assert('\0\0\0' <= '\0\0\0\0')
assert(not('\0\0\0\0' <= '\0\0\0'))
assert('\0\0\0' <= '\0\0\0')
assert('\0\0\0' >= '\0\0\0')
assert(not ('\0\0b' < '\0\0a\0'))
print("compares done")

print("about to enter %p block")
do  -- tests for '%p' format
  local null = "(null)"
  assert(string.format("%p", 4) == null)
  assert(string.format("%p", true) == null)
  assert(string.format("%p", nil) == null)
  assert(string.format("%p", {}) ~= null)
  assert(string.format("%p", print) ~= null)
  assert(string.format("%p", coroutine.running()) ~= null)
  assert(string.format("%p", io.stdin) ~= null)
  assert(string.format("%p", io.stdin) == string.format("%p", io.stdin))
  assert(string.format("%p", print) == string.format("%p", print))
  assert(string.format("%p", print) ~= string.format("%p", assert))
  print("L174 OK")

  assert(#string.format("%90p", {}) == 90)
  assert(#string.format("%-60p", {}) == 60)
  assert(string.format("%10p", false) == string.rep(" ", 10 - #null) .. null)
  assert(string.format("%-12p", 1.5) == null .. string.rep(" ", 12 - #null))
  print("L179 OK")

  do
    local t1 = {}; local t2 = {}
    assert(string.format("%p", t1) ~= string.format("%p", t2))
  end
  print("L183 OK")

  do
    local s1 = string.rep("a", 10)
    local s2 = string.rep("aa", 5)
    print("short s1 %p=" .. string.format("%p", s1))
    print("short s2 %p=" .. string.format("%p", s2))
    assert(string.format("%p", s1) == string.format("%p", s2))
  end
  print("L189 OK")

  do
    local s1 = string.rep("a", 300); local s2 = string.rep("a", 300)
    print("long s1 %p=" .. string.format("%p", s1))
    print("long s2 %p=" .. string.format("%p", s2))
    assert(string.format("%p", s1) ~= string.format("%p", s2))
  end
  print("L194 OK")
end
print("section1 done")
