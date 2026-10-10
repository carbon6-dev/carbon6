import unittest
from stats import moving_average

class T(unittest.TestCase):
    def test_basic(self):
        self.assertEqual(moving_average([1, 2, 3, 4], 2), [1.5, 2.5, 3.5])

    def test_full_window(self):
        self.assertEqual(moving_average([2, 4], 2), [3.0])

if __name__ == "__main__":
    unittest.main()
