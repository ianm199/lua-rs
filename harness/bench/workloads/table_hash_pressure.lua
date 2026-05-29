--[[
table_hash_pressure.lua — hash-part insertion under load.

Regression guard for issue #38: `getfreepos` used to scan the whole node
vector on every probe, making each hash insert O(n) and the table O(n^2).
This workload inserts 100k distinct string keys, deletes every third
(freeing hash nodes), reinserts them changed (exercising freed-slot reuse,
the exact path that regressed), then builds a second fresh 100k-key table.

If the quadratic ever comes back, the wall-time ratio vs reference C on the
dashboard blows up. Deterministic: asserts an exact checksum over both
tables.
]]

-- GC stopped so this isolates table hash-part insertion cost (the thing #38's
-- getfreepos fix targets). Collector throughput is covered by gc_pressure.lua.
collectgarbage("stop")

local N = 100000

local t = {}
for i = 1, N do t["k" .. i] = i end
for i = 1, N, 3 do t["k" .. i] = nil end   -- free every third hash node
for i = 1, N, 3 do t["k" .. i] = -i end    -- reinsert into freed slots

local sum = 0
for _, v in pairs(t) do sum = sum + v end

local t2 = {}
for i = 1, N do t2["x" .. i] = i end
local sum2 = 0
for _, v in pairs(t2) do sum2 = sum2 + v end

local expect = 0
for i = 1, N do
    if i % 3 == 1 then expect = expect - i else expect = expect + i end
end
assert(sum == expect, "table_hash_pressure sum mismatch: " .. sum .. " vs " .. expect)
assert(sum2 == N * (N + 1) // 2, "table_hash_pressure sum2 mismatch: " .. sum2)

io.write("table_hash_pressure.lua OK: sum=", sum, " sum2=", sum2, "\n")
