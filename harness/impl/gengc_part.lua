local co = coroutine.create(
  function ()
    local x = nil
    local f = function ()
                return x[1]
              end
    print("about to yield f, type=", type(f))
    x = coroutine.yield(f)
    print("resumed, x=", x)
    coroutine.yield()
  end
)
print("created co, type=", type(co))
local ok, f = coroutine.resume(co)
print("resume returned: ok=", ok, "f=", f, "type=", type(f))
