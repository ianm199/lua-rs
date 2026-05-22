-- harness preamble: emulate the globals lua-c testes/all.lua sets
_soft = true
_port = true
_nomsg = true
_U = false
arg = arg or {}
_G = _G or _ENV
if _VERSION == nil then _VERSION = "Lua 5.4" end

local function len (s)
  print("inside len, s type:", type(s), s)
  local x = string.gsub(s, "[\x80-\xBF]", "")
  print("inside len, x type:", type(x), x)
  return #x
end

print("len of hello:", len("hello"))

local function check (s)
  print("CHECK enter, len type:", type(len))
  local r = len(s)
  print("CHECK got r =", r)
end

check("hello World")
