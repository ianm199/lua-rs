-- harness preamble: emulate the globals lua-c testes/all.lua sets
_soft = true
_port = true
_nomsg = true
_U = false
arg = arg or {}
_G = _G or _ENV
if _VERSION == nil then _VERSION = "Lua 5.4" end

print('testing pattern matching debug')

local function f (s, p)
  local i,e = string.find(s, p)
  if i then return string.sub(s, i, e) end
end

print('A: gsub with global f')
function f(a,b) return string.gsub(a,'.',b) end
assert(string.gsub("trocar tudo em |teste|b| é |beleza|al|", "|([^|]*)|([^|]*)|", f) ==
            "trocar tudo em bbbbb é alalalalalal")

print('B: load+dostring test 1')
local function dostring (s) return load(s, "")() or "" end
print('B1: try simple load')
local fn = load("return 42", "")
print('B1: load returned type =', type(fn))
if fn then
  local v = fn()
  print('B1: fn() =', v)
end

print('B2: try load with a=x')
local fn2 = load("a='x'", "")
print('B2: load returned type =', type(fn2))
if fn2 then
  local v = fn2()
  print('B2: fn2() =', v, 'after a=', _G.a)
end

print('C: gsub with dostring')
assert(string.gsub("alo $a='x'$ novamente $return a$",
                   "$([^$]*)%$",
                   dostring) == "alo  novamente x")

print('done')
