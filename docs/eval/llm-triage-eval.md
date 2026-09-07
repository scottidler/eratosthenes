# LLM Triage Eval: 50-thread dry-run sample

**Date:** 2026-09-06  
**Binary:** eratosthenes at c6221ac + shellexpand fix  
**Model:** claude-haiku-4-5-20251001 via the keyless `claude` CLI transport (2.1.263)  
**Command:** `eratosthenes triage --dry-run`  
**Mutations:** zero (tool reported `50 threads classified, 0 labeled, 0 skipped (dry run)`)

## Measurement

- Full 50-thread run: **111.16s** wall clock (Gmail fetch ~52s + one batched classify call ~59s).
- Provisional triage transport timeout: 300s. Ratio **2.70x** measured, clears the doc's >= 2x gate. No amendment needed.
- 10-thread digest bullet pass: NOT MEASURED. That path ships in Phase 6; its 120s timeout is unconfirmed until then.
- Candidate pool was 401 threads; the `max-threads: 50` cap bit and said so loudly.

## Distribution

| bucket | count |
|---|---|
| fyi-work | 28 |
| recruiting | 17 |
| receipts | 2 |
| noise | 2 |
| needs-reply | 1 |

Total: 50

## Sign-off

Gate: **<= 5/50 disagreements**. Scott marks the `disagree?` column; any row he flips is a
bucket-description defect to iterate on, not a code defect.

Orchestrator pre-flagged 3 rows as debatable (marked `?`). They are the only rows where the
call turns on facts about Scott's own obligations rather than on the mail itself.

| # | thread | bucket | ? | subject |
|---|---|---|---|---|
| 1 | `1a0797a68f9574ab` | fyi-work |  | A new Amazon ElastiCache service update is available [AWS Account: 878256633362] |
| 2 | `1a0797a6bf20de2e` | fyi-work |  | A new Amazon ElastiCache service update is available [AWS Account: 878256633362] |
| 3 | `1a077ab8093b1eba` | receipts |  | Your payment has been processed for the invoice IN-008-015-416 |
| 4 | `1a077734d9d7ae50` | recruiting |  | Director – GPU Stack Unified Build & Release Platform at AMD: up to $370K/year |
| 5 | `1a076978c4d48db4` | recruiting |  | Reliability Engineering Manager: Ayar Labs hired near you |
| 6 | `1a0751ac414aa738` | fyi-work |  | Alert: User-reported phishing  for  janet@entevos.com |
| 7 | `1a074b65e82c794d` | fyi-work |  | Your certificate is renewed |
| 8 | `1a074b660a42f890` | fyi-work |  | Your certificate is renewed |
| 9 | `1a073bb8d539886f` | fyi-work |  | Issue Created: Red Agent discovered high severity vulnerability / misconfiguration |
| 10 | `1a073bb5daf0c5d0` | fyi-work |  | Issue Created: Validated exposure of sensitive data |
| 11 | `1a072bacce92bd22` | recruiting |  | Columbia Sportswear Company is hiring a Director, Infrastructure Engineering |
| 12 | `1a0724cee197c1ac` | recruiting |  | “platform director > 260k”: Columbia Sportswear Company - Director, Infrastructure Engineeri... |
| 13 | `1a07222a882be817` | fyi-work |  | Databricks – Auto-scoping complete for Your Databricks API Tokens |
| 14 | `1a071869d3a942ce` | fyi-work |  | Remember to Register a Backup MFA Verification Method |
| 15 | `1a071130fc452bba` | fyi-work |  | Issue Created: Validated exposure of SaaS API token |
| 16 | `1a070e2be34067a7` | fyi-work |  | Amazon EC2 Instance Retirement [AWS Account: 082130138416] |
| 17 | `1a06fef65919263a` | recruiting |  | Avinash Basani submitted a take home test for Senior Data Platform Engineer |
| 18 | `1a06f406bad1b59a` | fyi-work |  | [Notification] Amazon DocumentDB Service patch notification [AWS Account: 878256633362] |
| 19 | `1a06f406dce3b168` | fyi-work |  | [Notification] Amazon DocumentDB Service patch notification [AWS Account: 878256633362] |
| 20 | `1a06eceedb8b46a2` | noise |  | How the Brooklyn Nets save 150+ hours a month with Expensify |
| 21 | `1a06cdf9b5084edd` | fyi-work | **?** | ITHELP-5487 Hi IT (maybe Security?) - I'm trying to get access to our AWS Clean Room collab ... |
| 22 | `1a06e69c03691db3` | fyi-work |  | Notes: “AI Foundry” Sep 4, 2026 |
| 23 | `1a06e5f1ddf0ddd1` | recruiting |  | Yousef Gerfal was moved into Hiring Manager Review |
| 24 | `1a06e5e33b18644e` | recruiting |  | Brian Nguyen was moved into Hiring Manager Review |
| 25 | `1a06e5a1ff8b0a81` | recruiting |  | Kevin Chen was moved into Hiring Manager Review |
| 26 | `1a06e58f40121068` | recruiting |  | Roopsai Sarvepalli was moved into Hiring Manager Review |
| 27 | `1a06e56c96d4b104` | recruiting |  | Andrew Wei was moved into Hiring Manager Review |
| 28 | `1a06e54deb44777e` | recruiting |  | Thomas Lee was moved into Hiring Manager Review |
| 29 | `1a06e308762176da` | recruiting |  | New Remote roles near United States |
| 30 | `1a06e2d3be9cda80` | needs-reply |  | Investors for Tatari |
| 31 | `1a06cea2bd482727` | fyi-work |  | Automation rule 'Link issues that are mentioned in the...' failed |
| 32 | `1a06df54dacf22d0` | fyi-work |  | Rule triggered: Service account activity |
| 33 | `1a06df41a08f1327` | recruiting | **?** | REMINDER: Please fill out your scorecard for Mohan Atluri |
| 34 | `1a06db95e3778c68` | recruiting |  | Ram Vellamsetti submitted a take home test for Senior Data Platform Engineer |
| 35 | `1a06d9be128fb0e6` | recruiting |  | Fwd: Tatari Interview Confirmation \| Anil K |
| 36 | `1a06d94660e24e8a` | recruiting |  | “platform director > 260k”: KPMG US - Director, Platform Product Management posted on 9/3/26 |
| 37 | `1a06d5db8dad29d2` | fyi-work |  | Item shared with you: "mmz-csed-nbz (2026-09-03 09:58 GMT-7)" |
| 38 | `1a06d54851d0e3d0` | fyi-work |  | Notes: “Debrief: Teg (SVP, Product)” Sep 4, 2026 |
| 39 | `1a06d26915a64ff3` | recruiting |  | HHS Technology Group is hiring a (SR) Director Product Infrastructure Arch - REMOTE |
| 40 | `1a068bd09af4d5d1` | fyi-work | **?** | Re: Account Suppresion |
| 41 | `1a06d03ae7632262` | noise |  | Product updates - September 2026 |
| 42 | `1a06cf2d5eaa0a89` | fyi-work |  | Daily Proactive Checklist for Tatari   04 Sep 2026 |
| 43 | `1a06cdbd5bd92bcd` | receipts |  | [No Action Required]: Billing correction for a duplicate-processing issue on Claude.ai |
| 44 | `1a06b9ba9072e847` | fyi-work |  | [Action may be required] New S3 Data Events in AWS CloudTrail in 60 days [AWS Account: 87825... |
| 45 | `1a06b9bab5dba390` | fyi-work |  | [Action may be required] New S3 Data Events in AWS CloudTrail in 60 days [AWS Account: 87825... |
| 46 | `1a06a7b41f2b6e28` | fyi-work |  | Attention required on case 178760597200786: Quota Increase: RDS |
| 47 | `1a06a76c6c021111` | fyi-work |  | Your certificate is renewed |
| 48 | `1a06a76c749eef36` | fyi-work |  | Your certificate is renewed |
| 49 | `1a069978070f114c` | fyi-work |  | Scott Idler, here is your weekly update for 3 Sept |
| 50 | `1a0697aef2bcc7d9` | fyi-work |  | File detection File contains sensitive content and is accessible by the entire organization ... |

## Pre-flagged rows

- `1a06cdf9b5084edd` -> **fyi-work** -- asks IT/Security for access; if it is addressed to Scott it is an ask, not an FYI
  - subject: ITHELP-5487 Hi IT (maybe Security?) - I'm trying to get access to our AWS Clean Room collab with Amazon for AMC/Prime st
- `1a068bd09af4d5d1` -> **fyi-work** -- a `Re:` on an active thread; replies usually imply a pending turn
  - subject: Re: Account Suppresion
- `1a06df41a08f1327` -> **recruiting** -- REMINDER to fill out a scorecard is an action item for Scott, bucketed as recruiting
  - subject: REMINDER: Please fill out your scorecard for Mohan Atluri

**Status: awaiting Scott's sign-off.** Not self-certified.
