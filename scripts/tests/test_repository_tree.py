import subprocess
import unittest


class RepositoryTree(unittest.TestCase):
    def test_local_agent_worktrees_are_not_tracked(self):
        entries = subprocess.check_output(
            ["git", "ls-files", "--stage", "-z"], text=True
        ).split("\0")
        paths = [entry.split("\t", 1)[1] for entry in entries if entry]
        self.assertEqual(
            [path for path in paths if path.startswith(".claude/worktrees/")], []
        )


if __name__ == "__main__":
    unittest.main()
