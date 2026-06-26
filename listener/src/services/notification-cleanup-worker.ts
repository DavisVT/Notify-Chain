import logger from '../utils/logger';
import { CleanupConfig } from '../types';
import { ScheduledNotificationRepository } from './scheduled-notification-repository';

export interface CleanupCycleResult {
  notificationsDeleted: number;
  executionLogsDeleted: number;
}

/**
 * Background worker that removes expired or obsolete notification records
 * and execution logs based on configurable retention windows.
 */
export class NotificationCleanupWorker {
  private timer: NodeJS.Timeout | null = null;
  private isRunning = false;

  constructor(
    private readonly repository: ScheduledNotificationRepository,
    private readonly config: CleanupConfig,
  ) {}

  async start(): Promise<void> {
    if (this.isRunning) {
      logger.warn('Notification cleanup worker already running');
      return;
    }

    if (!this.config.enabled) {
      logger.info('Notification cleanup worker is disabled in configuration');
      return;
    }

    this.isRunning = true;
    logger.info('Starting notification cleanup worker', {
      pollIntervalMs: this.config.pollIntervalMs,
      notificationRetentionDays: this.config.notificationRetentionDays,
      executionLogRetentionDays: this.config.executionLogRetentionDays,
    });

    await this.runCycle();
    this.scheduleNext();
  }

  async stop(): Promise<void> {
    if (!this.isRunning) {
      return;
    }

    this.isRunning = false;

    if (this.timer) {
      clearTimeout(this.timer);
      this.timer = null;
    }

    logger.info('Notification cleanup worker stopped');
  }

  private scheduleNext(): void {
    if (!this.isRunning) {
      return;
    }

    this.timer = setTimeout(async () => {
      try {
        await this.runCycle();
      } catch (error) {
        logger.error('Notification cleanup worker cycle failed', { error });
      } finally {
        this.scheduleNext();
      }
    }, this.config.pollIntervalMs);

    this.timer.unref?.();
  }

  async runCycle(): Promise<CleanupCycleResult> {
    const notificationsDeleted = await this.repository.deleteExpiredNotifications(
      this.config.notificationRetentionDays,
    );
    const executionLogsDeleted = await this.repository.deleteExpiredExecutionLogs(
      this.config.executionLogRetentionDays,
    );

    logger.info('Notification cleanup cycle completed', {
      notificationsDeleted,
      executionLogsDeleted,
      notificationRetentionDays: this.config.notificationRetentionDays,
      executionLogRetentionDays: this.config.executionLogRetentionDays,
    });

    return { notificationsDeleted, executionLogsDeleted };
  }
}
