-- harness preamble: emulate the globals lua-c testes/all.lua sets
_soft = true
_port = true
_nomsg = true
_U = false
arg = arg or {}
_G = _G or _ENV
if _VERSION == nil then _VERSION = "Lua 5.4" end

local function checkerror (msg, f, ...)
  local s, err = pcall(f, ...)
  print("test:", msg, "s=", s, "err=", err)
  assert(not s and string.find(err, msg))
end

print("test 1:")
checkerror("invalid replacement value %(a table%)",
            string.gsub, "alo", ".", {a = {}})

print("test 2:")
checkerror("invalid capture index %%2", string.gsub, "alo", ".", "%2")

print("test 3:")
checkerror("invalid capture index %%0", string.gsub, "alo", "(%0)", "a")

print("test 4:")
checkerror("invalid capture index %%1", string.gsub, "alo", "(%1)", "a")

print("test 5:")
checkerror("invalid use of '%%'", string.gsub, "alo", ".", "%x")

print("all passed")
