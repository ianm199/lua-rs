-- harness preamble: emulate the globals lua-c testes/all.lua sets
_soft = true
_port = true
_nomsg = true
_U = false
arg = arg or {}
_G = _G or _ENV
if _VERSION == nil then _VERSION = "Lua 5.4" end

local function func2close(f)
  return setmetatable({}, {__close = f})
end

do
  -- previous test: error leaving a block
  local function foo (...)
    do
      local x1 <close> =
        func2close(function (self, msg)
          assert(string.find(msg, "@X"))
          error("@Y")
        end)

      local x123 <close> =
        func2close(function (_, msg)
          assert(msg == nil)
          error("@X")
        end)
    end
    os.exit(false)    -- should not run
  end

  local st, msg = xpcall(foo, debug.traceback)
  print("===@Y test===")
  print("st=", st)
  print("msg=", tostring(msg))
  print("match=", string.match(msg or "", "^[^ ]* @Y"))
end

do
  -- error in toclose in vararg function
  local function foo (...)
    local x123 <close> = func2close(function () error("@x123") end)
  end

  local st, msg = xpcall(foo, debug.traceback)
  print("===@x123 test===")
  print("st=", st)
  print("msg=", tostring(msg))
  print("match1=", string.match(msg or "", "^[^ ]* @x123"))
  print("find2=", string.find(msg or "", "in metamethod 'close'"))
end
