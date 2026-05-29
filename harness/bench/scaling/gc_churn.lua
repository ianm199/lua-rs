local N = tonumber(os.getenv("LUA_SCALING_N")) or 20000
local s = os.clock()
local acc = 0
for i = 1, N do local t = { i, i + 1, k = i }; acc = acc + t[1] + t.k end
io.write(string.format("time=%.6f n=%d acc=%d\n", os.clock() - s, N, acc))
