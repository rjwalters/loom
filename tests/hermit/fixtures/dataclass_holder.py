# Fixture: @dataclass classes -- should NOT be flagged (#13884)
# The generated __init__ IS the state; these classes have no visible
# self.x = assignment by design.
from dataclasses import dataclass


@dataclass
class Point:
    x: float
    y: float


@dataclass(frozen=True)
class FrozenPoint:
    x: float
    y: float
