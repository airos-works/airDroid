#!/usr/bin/env python3
"""airDroid's icon: a phone whose screen shows a little green robot head.
Rebuild with python3 tools/haiku/make-icon.py (writes resources/airDroid.hvif)."""
import math
from icon_art import Drawing, INK

d = Drawing()
d.shadow([(14,58),(44,58),(56,52),(26,52)])
# phone body (right-hand edge darker, as the Haiku style lights from top left)
d.shape([(14,6),(44,6),(48,10),(48,56),(44,60),(14,60),(10,56),(10,10)], d.gradient('5b6b7c','27313c'), INK)
# screen
d.shape([(14,12),(44,12),(44,52),(14,52)], d.gradient('d8f3ff','7fc4e8'), '3a4a5a', 1)
# robot head: a half disc with antennae and two eyes
cx, cy, r = 29, 38, 12
snap = lambda v: round(v * 2) / 2
arc = [(snap(cx + r * math.cos(math.pi + i * math.pi / 12)), snap(cy + r * math.sin(math.pi + i * math.pi / 12))) for i in range(13)]
d.shape(arc + [(cx + r, cy + 1), (cx - r, cy + 1)], d.gradient('b6ec6c','4f9a1f', (18,24), (40,40)), '2f5d12', 1.5)
d.line([(22,28),(18,22)], '2f5d12', 2)
d.line([(36,28),(40,22)], '2f5d12', 2)
d.ellipse(24, 33, 2, 2, 'ffffff')
d.ellipse(34, 33, 2, 2, 'ffffff')
# home button
d.ellipse(29, 56, 2, 1, '9aa8b6')
d.save(__file__, 'airdroid', 'airDroid')
