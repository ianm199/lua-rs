local N = tonumber(os.getenv("LUA_SCALING_N")) or 20000
collectgarbage("stop")
local s = os.clock()
local t = {}
for i = 1, N do t[i] = i end
io.write(string.format("time=%.6f n=%d\n", os.clock() - s, N))
