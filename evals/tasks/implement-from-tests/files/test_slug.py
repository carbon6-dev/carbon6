import unittest
from slug import slugify

class T(unittest.TestCase):
    def test_simple(self):
        self.assertEqual(slugify("Hello World"), "hello-world")

    def test_punctuation(self):
        self.assertEqual(slugify("  Rust & Leptos: a guide!  "), "rust-leptos-a-guide")

    def test_repeats(self):
        self.assertEqual(slugify("a---b___c"), "a-b-c")

    def test_empty(self):
        self.assertEqual(slugify("!!!"), "")

if __name__ == "__main__":
    unittest.main()
