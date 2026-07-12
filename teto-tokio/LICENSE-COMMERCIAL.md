# Commercial License for teto-tokio

## Overview

teto-tokio is **dual-licensed**. You may use it under **either** of the
following licenses — the choice is entirely yours:

1. **GNU Affero General Public License v3.0 (AGPL-3.0-only)** — free of charge,
   for anyone, for any purpose, including commercial use. See [LICENSE](LICENSE)
   for the full text.
2. **A commercial license** — a proprietary agreement for those who do not wish
   to comply with the AGPL-3.0 copyleft obligations.

You only need one. There are no restrictions based on company size, headcount,
or revenue: **anyone may use teto-tokio for free under the AGPL-3.0**, provided
they meet its terms.

## When would I want a commercial license?

The AGPL-3.0 is a strong "network copyleft" license, and its **Section 13** is
especially relevant here: teto-tokio is an async adapter for teto-dpdk, designed
to run as network infrastructure. If you modify teto-tokio and either distribute
it or make it available to users over a network, the AGPL-3.0 requires you to
offer the **complete corresponding source code** of your work, under the
AGPL-3.0, to those users.

A commercial license may be the better fit if you want to:

- Use teto-tokio in a **proprietary or closed-source** product without the
  AGPL-3.0 copyleft obligations;
- Avoid the requirement to disclose the source code of your own application;
- Avoid the AGPL-3.0 network-use source-disclosure requirement (Section 13),
  which is particularly relevant since teto-tokio runs as network infrastructure;
- Obtain priority support or other terms not offered under the open-source
  license (depending on the tier).

If you are comfortable meeting the AGPL-3.0 obligations, you never need to
purchase a commercial license.

## What the commercial license provides

- Permission to use teto-tokio in proprietary, closed-source applications
  without the AGPL-3.0 copyleft requirements;
- Freedom from the requirement to disclose your source code;
- Freedom from the AGPL-3.0 network-use source disclosure requirement
  (Section 13);
- Priority support options (depending on the license tier).

## Third-Party Dependency Notices

teto-tokio builds on teto-dpdk, which statically links against the following
BSD-licensed libraries. These licenses are permissive and pass through to all
users — open-source and commercial alike. Compliance requires only retaining the
copyright notices:

- **F-Stack** — BSD 2-Clause License. Copyright (C) 2017–2022 THL A29 Limited, a Tencent company.
  Source: <https://github.com/F-Stack/f-stack>

- **DPDK** — BSD 3-Clause License. Copyright (c) 2010–present, Intel Corporation and contributors.
  Source: <https://www.dpdk.org>

A commercial license from mehve-labs covers **only the teto-tokio and teto-dpdk
code**; it does not and cannot waive these upstream BSD obligations, which
continue to apply to the F-Stack and DPDK components.

## Contact

For commercial licensing inquiries, please contact:

**Email:** mehvelabs@gmail.com

## Disclaimer

This document is a summary of the commercial licensing terms and is provided for
informational purposes only. The actual commercial license agreement will be
provided upon purchase.
