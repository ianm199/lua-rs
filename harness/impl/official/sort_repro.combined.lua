_soft = true
_port = true
_nomsg = true
_U = false
arg = arg or {}
_G = _G or _ENV
if _VERSION == nil then _VERSION = "Lua 5.4" end

print("calling collectgarbage 1")
collectgarbage()
print("done 1")
collectgarbage()
print("done 2")
collectgarbage()
print("done 3")
collectgarbage()
print("done 4")

AA = {"a", "b", "c", "d", "e", "f"}
table.sort(AA, function (x, y)
          collectgarbage()
          return x<y
        end)
print("step 2 ok")
