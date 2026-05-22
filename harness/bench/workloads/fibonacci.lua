--[[
fibonacci.lua — recursive function call overhead + small-integer math.

Measures: call/return dispatch, integer addition, recursion depth handling.
Tight loop, no allocation outside the call stack.

Workload: fib(34) computed 10 times. Reference C Lua should complete in ~3 s
on Apple Silicon; lua-rs latency is the headline metric.

Deterministic checksum: fib(34) = 5702887. Multiplied by 10 = 57028870.
]]

local function fib(n)
    if n < 2 then return n end
    return fib(n - 1) + fib(n - 2)
end

local total = 0
for _ = 1, 10 do
    total = total + fib(34)
end

assert(total == 57028870, "fibonacci checksum mismatch: got " .. total)
io.write("fibonacci.lua OK: ", total, "\n")
