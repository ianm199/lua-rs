--[[
mandelbrot.lua — float math + nested loops + integer bitwise output packing.

Measures: float arithmetic per iteration, branch prediction in the bailout
check, no allocation in the hot loop.

Workload: 400x400 grid, max 50 iterations per pixel. Result is summed into
a fingerprint so the compiler cannot dead-code the loop.

Deterministic: at 400x400 / iter 50, the in-set pixel count is stable.
]]

local size = 400
local max_iter = 50
local in_set = 0

for py = 0, size - 1 do
    local y0 = (py / size) * 2.0 - 1.0
    for px = 0, size - 1 do
        local x0 = (px / size) * 3.0 - 2.0
        local x, y = 0.0, 0.0
        local iter = 0
        while x * x + y * y <= 4.0 and iter < max_iter do
            local xt = x * x - y * y + x0
            y = 2.0 * x * y + y0
            x = xt
            iter = iter + 1
        end
        if iter == max_iter then in_set = in_set + 1 end
    end
end

assert(in_set == 42461, "mandelbrot checksum mismatch: got " .. in_set)
io.write("mandelbrot.lua OK: in_set=", in_set, "\n")
