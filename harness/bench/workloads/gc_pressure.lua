--[[
gc_pressure.lua — allocation and GC throughput under churn.

Allocates 200k short-lived mixed tables (array slots + hash fields) so the
incremental collector has to keep pace with steady garbage, with periodic
explicit steps. Tracks GC/allocator throughput on the dashboard; a
regression in collection or allocation cost shows as a worse wall ratio.

Deterministic: the accumulator folds in fields from every table so the work
can't be optimized away, and the total is a closed form we assert.
]]

local iters = 200000
local acc = 0
for i = 1, iters do
    local t = { i, i + 1, i + 2, k = i, n = i + 1 }
    acc = acc + t[1] + t.k
    if i % 4096 == 0 then collectgarbage("step", 0) end
end

-- acc = sum over i of (t[1] + t.k) = sum of (i + i) = 2 * sum(1..iters)
--     = iters * (iters + 1)
assert(acc == iters * (iters + 1), "gc_pressure acc mismatch: " .. acc)

io.write("gc_pressure.lua OK: acc=", acc, "\n")
