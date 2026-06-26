import * as fs from 'fs';
import * as path from 'path';
import { Database } from '../database/database';
import { ScheduledNotificationRepository } from './scheduled-notification-repository';
import { NotificationCleanupWorker } from './notification-cleanup-worker';
import { NotificationStatus, NotificationType } from '../types/scheduled-notification';
import { CleanupConfig } from '../types';

jest.mock('../utils/logger', () => ({
  __esModule: true,
  default: {
    info: jest.fn(),
    warn: jest.fn(),
    error: jest.fn(),
    debug: jest.fn(),
  },
}));

describe('NotificationCleanupWorker', () => {
  const dbPath = path.join(__dirname, '../../data/test-cleanup-worker.db');
  let db: Database;
  let repository: ScheduledNotificationRepository;
  const config: CleanupConfig = {
    enabled: true,
    pollIntervalMs: 60_000,
    notificationRetentionDays: 30,
    executionLogRetentionDays: 90,
  };

  beforeEach(async () => {
    if (fs.existsSync(dbPath)) {
      fs.unlinkSync(dbPath);
    }
    db = new Database(dbPath);
    await db.initialize();
    repository = new ScheduledNotificationRepository(db);
  });

  afterEach(async () => {
    await db.close();
    if (fs.existsSync(dbPath)) {
      fs.unlinkSync(dbPath);
    }
  });

  async function seedCompletedNotification(updatedAt: string): Promise<number> {
    const result = await db.run(
      `INSERT INTO scheduled_notifications (
        payload, notification_type, target_recipient, execute_at, status, updated_at
      ) VALUES (?, ?, ?, ?, ?, ?)`,
      [
        JSON.stringify({ message: 'test' }),
        NotificationType.DISCORD,
        'recipient-1',
        updatedAt,
        NotificationStatus.COMPLETED,
        updatedAt,
      ],
    );
    return result.lastID;
  }

  it('deletes expired terminal notifications and logs cleanup results', async () => {
    const oldDate = '2020-01-01T00:00:00.000Z';
    const recentDate = new Date().toISOString();

    await seedCompletedNotification(oldDate);
    await seedCompletedNotification(recentDate);

    const worker = new NotificationCleanupWorker(repository, config);
    const result = await worker.runCycle();

    expect(result.notificationsDeleted).toBe(1);

    const remaining = await db.all('SELECT id FROM scheduled_notifications');
    expect(remaining).toHaveLength(1);
  });

  it('deletes expired execution logs', async () => {
    const notificationId = await seedCompletedNotification(new Date().toISOString());

    await db.run(
      `INSERT INTO notification_execution_log (
        scheduled_notification_id, execution_attempt, status, execution_time
      ) VALUES (?, ?, ?, ?)`,
      [notificationId, 1, 'FAILED', '2020-01-01T00:00:00.000Z'],
    );

    const worker = new NotificationCleanupWorker(repository, config);
    const result = await worker.runCycle();

    expect(result.executionLogsDeleted).toBe(1);
  });

  it('does not start when disabled', async () => {
    const worker = new NotificationCleanupWorker(repository, {
      ...config,
      enabled: false,
    });

    await worker.start();
    const result = await worker.runCycle();
    expect(result.notificationsDeleted).toBe(0);
    await worker.stop();
  });
});
