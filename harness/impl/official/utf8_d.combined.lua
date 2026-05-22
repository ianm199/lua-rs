-- harness preamble: emulate the globals lua-c testes/all.lua sets
_soft = true
_port = true
_nomsg = true
_U = false
arg = arg or {}
_G = _G or _ENV
if _VERSION == nil then _VERSION = "Lua 5.4" end

-- bisect: full content up to line 146 inclusive

print "testing UTF-8 library"

local utf8 = require'utf8'


local function checkerror (msg, f, ...)
  local s, err = pcall(f, ...)
  assert(not s and string.find(err, msg))
end


local function len (s)
  return #string.gsub(s, "[\x80-\xBF]", "")
end


local justone = "^" .. utf8.charpattern .. "$"

-- 't' is the list of codepoints of 's'
local function checksyntax (s, t)
  -- creates a string "return '\u{t[1]}...\u{t[n]}'"
  local ts = {"return '"}
  for i = 1, #t do ts[i + 1] = string.format("\\u{%x}", t[i]) end
  ts[#t + 2] = "'"
  ts = table.concat(ts)
  -- its execution should result in 's'
  assert(assert(load(ts))() == s)
end

assert(not utf8.offset("alo", 5))
assert(not utf8.offset("alo", -4))

print "checkpoint 1"

-- 'check' makes several tests over the validity of string 's'.
-- 't' is the list of codepoints of 's'.
local function check (s, t, nonstrict)
  local l = utf8.len(s, 1, -1, nonstrict)
  assert(#t == l and len(s) == l)
  assert(utf8.char(table.unpack(t)) == s)   -- 't' and 's' are equivalent

  assert(utf8.offset(s, 0) == 1)

  checksyntax(s, t)

  -- creates new table with all codepoints of 's'
  local t1 = {utf8.codepoint(s, 1, -1, nonstrict)}
  assert(#t == #t1)
  for i = 1, #t do assert(t[i] == t1[i]) end   -- 't' is equal to 't1'

  for i = 1, l do   -- for all codepoints
    local pi = utf8.offset(s, i)        -- position of i-th char
    local pi1 = utf8.offset(s, 2, pi)   -- position of next char
    assert(string.find(string.sub(s, pi, pi1 - 1), justone))
    assert(utf8.offset(s, -1, pi1) == pi)
    assert(utf8.offset(s, i - l - 1) == pi)
    assert(pi1 - pi == #utf8.char(utf8.codepoint(s, pi, pi, nonstrict)))
    for j = pi, pi1 - 1 do
      assert(utf8.offset(s, 0, j) == pi)
    end
    for j = pi + 1, pi1 - 1 do
      assert(not utf8.len(s, j))
    end
   assert(utf8.len(s, pi, pi, nonstrict) == 1)
   assert(utf8.len(s, pi, pi1 - 1, nonstrict) == 1)
   assert(utf8.len(s, pi, -1, nonstrict) == l - i + 1)
   assert(utf8.len(s, pi1, -1, nonstrict) == l - i)
   assert(utf8.len(s, 1, pi, nonstrict) == i)
  end

  local i = 0
  for p, c in utf8.codes(s, nonstrict) do
    i = i + 1
    assert(c == t[i] and p == utf8.offset(s, i))
    assert(utf8.codepoint(s, p, p, nonstrict) == c)
  end
  assert(i == #t)

  i = 0
  for c in string.gmatch(s, utf8.charpattern) do
    i = i + 1
    assert(c == utf8.char(t[i]))
  end
  assert(i == #t)

  for i = 1, l do
    assert(utf8.offset(s, i) == utf8.offset(s, i - l - 1, #s + 1))
  end

end


do    -- error indication in utf8.len
  local function check (s, p)
    local a, b = utf8.len(s)
    assert(not a and b == p)
  end
  check("abc\xE3def", 4)
  check("\xF4\x9F\xBF", 1)
  check("\xF4\x9F\xBF\xBF", 1)
  -- spurious continuation bytes
  check("汉字\x80", #("汉字") + 1)
  check("\x80hello", 1)
  check("hel\x80lo", 4)
  check("汉字\xBF", #("汉字") + 1)
  check("\xBFhello", 1)
  check("hel\xBFlo", 4)
end

print "checkpoint 2"

-- errors in utf8.codes
do
  local function errorcodes (s)
    checkerror("invalid UTF%-8 code",
      function ()
        for c in utf8.codes(s) do assert(c) end
      end)
  end
  errorcodes("ab\xff")
  errorcodes("\u{110000}")
  errorcodes("in\x80valid")
  errorcodes("\xbfinvalid")
  errorcodes("αλφ\xBFα")

  -- calling interation function with invalid arguments
  local f = utf8.codes("")
  assert(f("", 2) == nil)
  assert(f("", -1) == nil)
  assert(f("", math.mininteger) == nil)

end

print "checkpoint 3"

-- error in initial position for offset
checkerror("position out of bounds", utf8.offset, "abc", 1, 5)
checkerror("position out of bounds", utf8.offset, "abc", 1, -4)
checkerror("position out of bounds", utf8.offset, "", 1, 2)
checkerror("position out of bounds", utf8.offset, "", 1, -1)
checkerror("continuation byte", utf8.offset, "𦧺", 1, 2)
checkerror("continuation byte", utf8.offset, "𦧺", 1, 2)
checkerror("continuation byte", utf8.offset, "\x80", 1)

-- error in indices for len
checkerror("out of bounds", utf8.len, "abc", 0, 2)
checkerror("out of bounds", utf8.len, "abc", 1, 4)

print "checkpoint 4"

local s = "hello World"
local t = {string.byte(s, 1, -1)}
for i = 1, utf8.len(s) do assert(t[i] == string.byte(s, i)) end
check(s, t)

print "checkpoint 5"

check("汉字/漢字", {27721, 23383, 47, 28450, 23383,})

print "checkpoint 6"
