--[[
closure_ops.lua — closure creation and invocation, upvalue access.

Measures: closure allocation cost, upvalue read/write through the upvalue
indirection layer, captured-state lifetime under GC.

Workload: 100000 counter closures, each invoked 100 times. The counter
captures one upvalue (a number).

Deterministic: sum of final counter values is N * iterations.
]]

local function make_counter(start)
    local n = start
    return function()
        n = n + 1
        return n
    end
end

local total = 0
local counters = {}
for i = 1, 100000 do counters[i] = make_counter(0) end

for i = 1, 100000 do
    local c = counters[i]
    local last = 0
    for _ = 1, 100 do last = c() end
    total = total + last
end

assert(total == 10000000,
       "closure_ops checksum mismatch: got " .. total)
io.write("closure_ops.lua OK: total=", total, "\n")
