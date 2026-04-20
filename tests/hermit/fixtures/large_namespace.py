# Fixture: Large namespace class (10+ methods) -- should NOT be flagged
class MathUtils:
    def add(self, a, b): return a + b
    def subtract(self, a, b): return a - b
    def multiply(self, a, b): return a * b
    def divide(self, a, b): return a / b
    def modulo(self, a, b): return a % b
    def power(self, a, b): return a ** b
    def floor_div(self, a, b): return a // b
    def negate(self, a): return -a
    def absolute(self, a): return abs(a)
    def maximum(self, a, b): return max(a, b)
    def minimum(self, a, b): return min(a, b)
