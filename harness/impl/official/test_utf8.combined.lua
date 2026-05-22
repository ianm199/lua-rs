-- harness preamble: emulate the globals lua-c testes/all.lua sets
_soft = true
_port = true
_nomsg = true
_U = false
arg = arg or {}
_G = _G or _ENV
if _VERSION == nil then _VERSION = "Lua 5.4" end

local utf8 = require("utf8")

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

print("first assertions ok")

local function check (s, t, nonstrict)
  local l = utf8.len(s, 1, -1, nonstrict)
  print("got l =", l, "expected", #t)
  assert(#t == l and len(s) == l)
end

do    -- error indication in utf8.len
  local function check2 (s, p)
    local a, b = utf8.len(s)
    assert(not a and b == p)
  end
  check2("abc\xE3def", 4)
  check2("\xF4\x9F\xBF", 1)
  check2("\xF4\x9F\xBF\xBF", 1)
end
print("done do block")
