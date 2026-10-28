// Tests are never scanned for registrations.
import { udf } from './udf.js';

udf.func('ignoredInTests', async () => 0);
