-- harness preamble: emulate the globals lua-c testes/all.lua sets
_soft = true
_port = true
_nomsg = true
_U = false
arg = arg or {}
_G = _G or _ENV
if _VERSION == nil then _VERSION = "Lua 5.4" end

local utf8 = require'utf8'

local function checkerror (msg, f, ...)
  local s, err = pcall(f, ...)
  assert(not s and string.find(err, msg))
end

local function len (s)
  return #string.gsub(s, "[\x80-\xBF]", "")
end

local justone = "^" .. utf8.charpattern .. "$"

local function checksyntax (s, t)
  local ts = {"return '"}
  for i = 1, #t do ts[i + 1] = string.format("\\u{%x}", t[i]) end
  ts[#t + 2] = "'"
  ts = table.concat(ts)
  print("loading: ", ts)
  local f, err = load(ts)
  print("loaded:", f, err)
  assert(f, err)
  print("call result", f() == s)
end

local s = "hello World"
local t = {string.byte(s, 1, -1)}

-- now do step-by-step of check()
local nonstrict = nil
local l = utf8.len(s, 1, -1, nonstrict)
print("step 1: l=", l)
assert(#t == l and len(s) == l)
print("step 2: ok")
assert(utf8.char(table.unpack(t)) == s)
print("step 3: ok")
assert(utf8.offset(s, 0) == 1)
print("step 4: ok")
checksyntax(s, t)
print("step 5: checksyntax done")

local t1 = {utf8.codepoint(s, 1, -1, nonstrict)}
print("step 6: t1 #", #t1)
assert(#t == #t1)
for i = 1, #t do assert(t[i] == t1[i]) end
print("step 7: codepoints match")

for i = 1, l do   -- for all codepoints
    local pi = utf8.offset(s, i)        -- position of i-th char
    local pi1 = utf8.offset(s, 2, pi)   -- position of next char
    print("i=", i, "pi=", pi, "pi1=", pi1)
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
print("step 8: per-char loop done")

local i = 0
for p, c in utf8.codes(s, nonstrict) do
  i = i + 1
  assert(c == t[i] and p == utf8.offset(s, i))
  assert(utf8.codepoint(s, p, p, nonstrict) == c)
end
assert(i == #t)
print("step 9: codes iter done")

i = 0
for c in string.gmatch(s, utf8.charpattern) do
  i = i + 1
  assert(c == utf8.char(t[i]))
end
assert(i == #t)
print("step 10: gmatch loop done")

for i = 1, l do
  assert(utf8.offset(s, i) == utf8.offset(s, i - l - 1, #s + 1))
end
print("all done")
