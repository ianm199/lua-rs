local N = tonumber(os.getenv("LUA_SCALING_N")) or 20000
collectgarbage("stop")
local t = {}
for i = 1, N do t["k" .. i] = i end
local s = os.clock()
local sum = 0
for _, v in pairs(t) do sum = sum + v end
io.write(string.format("time=%.6f n=%d sum=%d\n", os.clock() - s, N, sum))
