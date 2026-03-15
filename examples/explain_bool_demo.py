#!/usr/bin/env python3
"""Example: Using explain parameter with bool values.

Demonstrates the improved explain parameter that now accepts both bool and str values.
"""

import requests

BASE_URL = "http://localhost:8100/v1"
TOKEN = "test-master-key-for-docker-compose"

headers = {"Authorization": f"Bearer {TOKEN}"}


def test_explain_bool():
    """Test explain parameter with bool values."""

    # Store a test memory
    store_resp = requests.post(
        f"{BASE_URL}/memories",
        json={"content": "Test memory for explain demo", "memory_type": "semantic"},
        headers=headers,
    )
    print(f"✓ Stored memory: {store_resp.status_code}")

    # Test 1: explain=false (no explain output)
    print("\n1. Testing explain=false (should have no explain output):")
    resp = requests.post(
        f"{BASE_URL}/memories/search",
        json={"query": "test", "explain": False},
        headers=headers,
    )
    result = resp.json()
    print(f"   Has explain? {'explain' in result}")

    # Test 2: explain=true (basic explain output)
    print("\n2. Testing explain=true (should have basic explain output):")
    resp = requests.post(
        f"{BASE_URL}/memories/search",
        json={"query": "test", "explain": True},
        headers=headers,
    )
    result = resp.json()
    if "explain" in result:
        print(f"   Explain level: {result['explain']['level']}")
        print(f"   Total time: {result['explain']['total_ms']:.2f}ms")

    # Test 3: explain="verbose" (detailed explain output)
    print("\n3. Testing explain='verbose' (should have detailed explain output):")
    resp = requests.post(
        f"{BASE_URL}/memories/search",
        json={"query": "test", "explain": "verbose"},
        headers=headers,
    )
    result = resp.json()
    if "explain" in result:
        print(f"   Explain level: {result['explain']['level']}")
        print(f"   Has metrics? {'metrics' in result['explain']}")

    # Test 4: explain="none" (no explain output)
    print("\n4. Testing explain='none' (should have no explain output):")
    resp = requests.post(
        f"{BASE_URL}/memories/search",
        json={"query": "test", "explain": "none"},
        headers=headers,
    )
    result = resp.json()
    print(f"   Has explain? {'explain' in result}")


if __name__ == "__main__":
    print("Testing improved explain parameter (bool + str support)\n")
    print("=" * 60)
    test_explain_bool()
    print("\n" + "=" * 60)
    print("✓ All tests completed!")
