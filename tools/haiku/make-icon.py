#!/usr/bin/env python3
"""Original Haiku clapperboard with a play mark, FilmCraft's air/OS icon (MIT OR Apache-2.0)."""
from icon_art import Drawing, INK, plane

d = Drawing()
# The board: a slab on a gently receding plane, its right face darker (light from the top left).
p = plane(6, 20, 0.3)
d.shadow([(8,52),(48,62),(62,62),(34,52)])
d.shape([(46,32),(51,29),(51,59),(46,62)], d.gradient('3c434f','1d2128'), INK)
d.shape(p([(0,0),(40,0),(40,30),(0,30)]), d.gradient('5d6676','2b3039'), INK)
# The striped band along the board's top edge.
d.shape(p([(0,0),(40,0),(40,7),(0,7)]), d.solid('f2f2ec'))
for x in (2, 12, 22, 32):
    d.shape(p([(x,0),(x+5,0),(x+2,7),(x-3,7)]), d.solid('1d2128'))
d.shape(p([(0,0),(40,0),(40,7),(0,7)]), None, INK, 1.5)
# The play mark: what FilmCraft does with the footage.
d.shape(p([(15,11),(15,26),(28,18.5)]), d.gradient('ffd77a','e0782c', (20,30),(34,48)), INK, 1.5)
# The clapper stick, raised on its hinge at the left.
A, L, W = (4, 17), (40, -12), (2, 6)
def at(t, side=0, slant=0.0):
    return (A[0] + L[0]*(t+slant*side) + W[0]*side, A[1] + L[1]*(t+slant*side) + W[1]*side)
d.shape([at(0), at(1), at(1,1), at(0,1)], d.solid('f2f2ec'), INK, 1.8)
for t0, t1 in ((0.08, 0.22), (0.36, 0.50), (0.64, 0.78)):
    d.shape([at(t0), at(t1), at(t1,1,-0.1), at(t0,1,-0.1)], d.solid('1d2128'))
d.shape([at(0), at(1), at(1,1), at(0,1)], None, INK, 1.8)
d.ellipse(6, 20, 2, 2, d.solid('9aa3ad'), INK, 1)
d.save(__file__, 'filmcraft', 'FilmCraft')
