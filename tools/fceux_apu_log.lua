-- Dump every APU register write ($4000-$4017) as `frame addr value`, one
-- per line, so `viper verify song.nsf --against fceux.log` can diff
-- FCEUX's playback of an NSF against viper-apu's.
--
--   FCEUX_LOG=out.log FCEUX_FRAMES=300 \
--     xvfb-run -a fceux --loadlua tools/fceux_apu_log.lua song.nsf
--
-- FCEUX numbers frames from its own counter and clears the APU itself
-- before INIT; `viper verify` lines the frame numbers up and treats the
-- INIT frame as a set, so neither needs fixing up here.

local path = os.getenv("FCEUX_LOG") or "fceux_apu.log"
local max_frames = tonumber(os.getenv("FCEUX_FRAMES") or "600")
local f = assert(io.open(path, "w"))

local function on_write(addr, size, value)
  f:write(string.format("%d %04X %02X\n", emu.framecount(), addr, value))
end

for addr = 0x4000, 0x4017 do
  memory.registerwrite(addr, 1, on_write)
end

local start = emu.framecount()
while emu.framecount() - start < max_frames do
  emu.frameadvance()
end
f:close()
emu.exit()
