--[[
table_ops.lua — table insert/remove/iterate; array part + hash part mix.

Measures: array growth strategy, hash slot probing, ipairs/pairs traversal,
table.remove shift cost.

Workload: build a 10000-element array, do 1000 inserts + removes at
random positions, then iterate twice (ipairs + pairs over a 1000-key hash
table).

Deterministic: sums all values seen during iteration.
]]

local arr = {}
for i = 1, 10000 do arr[i] = i end

local removed_sum = 0
math.randomseed(42)
for _ = 1, 1000 do
    local pos = math.random(1, #arr)
    removed_sum = removed_sum + table.remove(arr, pos)
end

for i = 1, 1000 do
    table.insert(arr, math.random(1, 10), i * 2)
end

local ipairs_sum = 0
for _, v in ipairs(arr) do ipairs_sum = ipairs_sum + v end

local hash = {}
for i = 1, 1000 do hash["key_" .. i] = i * 3 end
local pairs_sum = 0
for _, v in pairs(hash) do pairs_sum = pairs_sum + v end

assert(pairs_sum == 1501500,
       "table_ops pairs_sum mismatch: got " .. pairs_sum)
io.write("table_ops.lua OK: ipairs_sum=", ipairs_sum,
         " pairs_sum=", pairs_sum,
         " removed_sum=", removed_sum, "\n")
