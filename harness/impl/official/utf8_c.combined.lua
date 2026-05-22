-- harness preamble: emulate the globals lua-c testes/all.lua sets
_soft = true
_port = true
_nomsg = true
_U = false
arg = arg or {}
_G = _G or _ENV
if _VERSION == nil then _VERSION = "Lua 5.4" end

-- UTF-8 file

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

-- errors in utf8.codes
do
  local function errorcodes (s)
    checkerror("invalid UTF%-8 code",
      function ()
        for c in utf8.codes(s) do assert(c) end
      end)
  end
  print "before errorcodes 1"
  errorcodes("ab\xff")
  print "after errorcodes 1"
  errorcodes("\u{110000}")
  print "after errorcodes 2"
  errorcodes("in\x80valid")
  errorcodes("\xbfinvalid")
  errorcodes("αλφ\xBFα")

  -- calling interation function with invalid arguments
  local f = utf8.codes("")
  assert(f("", 2) == nil)
  assert(f("", -1) == nil)
  assert(f("", math.mininteger) == nil)

end
print "after do block"
