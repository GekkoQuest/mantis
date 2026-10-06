# std.containers rules

- Every character has one bag of 20 slots and a gold balance. A slot holds
  one stack: an item (a content id, never 0) and a count from 1 to 99.
- Every change to a bag is an economy command: logged, executed on
  delivery, its outcome logged with the ledger rows of what changed hands
  (decision 0007). A refused command changes nothing: changes run inside
  `Inventories::transaction`, which restores every bag it touched on error.
- Moving a stack onto a stack of the same item merges up to 99, leaving the
  rest; onto anything else it swaps.
- Splitting moves part of a stack (at least 1, fewer than all) into an
  empty slot.
- Destroying removes up to the whole stack; the ledger records the loss.
- Grants (items and gold) come only from services: a command carrying a
  client session is refused. Granted items top up existing stacks first.
- After every change the owner receives its bag.
- Other economy modules (vendor, mail, auction) change bags through the
  same transaction API, inside their own commands, so a trade is all or
  nothing across every bag it touches.
