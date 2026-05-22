print("testing tail calls")

function deep (n) if n>0 then return deep(n-1) else return 101 end end
print("a:", deep(30000))
