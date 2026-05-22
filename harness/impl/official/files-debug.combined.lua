_soft = true
_port = true
_nomsg = true
_U = false
arg = arg or {}
_G = _G or _ENV
if _VERSION == nil then _VERSION = "Lua 5.4" end

print("testing")

local function testerr (msg, f, ...)
  local stat, err = pcall(f, ...)
  print("testerr stat=", stat, "err=", err)
  return (not stat and string.find(err, msg, 1, true))
end


local function checkerr (msg, f, ...)
  assert(testerr(msg, f, ...))
end

checkerr("invalid conversion specifier", os.date, "%")
checkerr("invalid conversion specifier", os.date, "%9")
checkerr("invalid conversion specifier", os.date, "%")
checkerr("invalid conversion specifier", os.date, "%O")

print("done")
