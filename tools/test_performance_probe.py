"""Regression checks for performance acceptance, without launching any services."""
import copy
import unittest
from performance_probe import healthy_accounts, measurement_result


def fixture():
    status = dict(account_sync=[dict(account=str(i), paused=False, status='已更新 1 个文件夹') for i in range(5)])
    sample = dict(elapsed_seconds=0, cpu_seconds=1, pss_kib=16*1024,
                  status=status, fixture_services_alive=True, imap_commands={})
    last = copy.deepcopy(sample)
    last.update(elapsed_seconds=1800, cpu_seconds=8)
    return dict(stabilization_seconds=300, samples=[sample, last])


class Acceptance(unittest.TestCase):
    def test_stable_accounts_with_periodic_sync_are_eligible(self):
        report = fixture()
        report['samples'][0]['status']['account_sync'][2]['status'] = '正在同步'
        self.assertTrue(measurement_result(report, 5)['budget_passed'])

    def test_keyring_crash_and_locked_replacement_cannot_pass_low_cpu(self):
        for failure in ('dead_service', 'locked_replacement'):
            with self.subTest(failure=failure):
                report = fixture()
                last = report['samples'][-1]
                if failure == 'dead_service':
                    last['fixture_services_alive'] = False
                else:
                    last['status']['account_sync'][0]['status'] = '密钥库已锁定；独立退避后重试'
                result = measurement_result(report, 5)
                self.assertTrue(result['resource_numbers_within_budget'])
                self.assertFalse(result['stable_fixture'])
                self.assertFalse(result['budget_passed'])

    def test_missing_duplicate_or_paused_account_is_not_five_stable_accounts(self):
        states = fixture()['samples'][0]['status']
        for failure in ('missing', 'duplicate', 'paused'):
            state = copy.deepcopy(states)
            if failure == 'missing': state['account_sync'].pop()
            elif failure == 'duplicate': state['account_sync'][1]['account'] = '0'
            else: state['account_sync'][0]['paused'] = True
            self.assertFalse(healthy_accounts(state))

    def test_short_or_sparse_measurement_cannot_qualify(self):
        report = fixture()
        self.assertFalse(measurement_result(report, 60)['budget_passed'])
        report['stabilization_seconds'] = 299
        self.assertFalse(measurement_result(report, 5)['budget_passed'])
        report['stabilization_seconds'] = 300
        report['samples'][-1]['elapsed_seconds'] = 1799
        self.assertFalse(measurement_result(report, 5)['budget_passed'])

    def test_error_or_resource_peak_is_not_averaged_away(self):
        for failure in ('protocol', 'memory', 'cpu'):
            report = fixture()
            last = report['samples'][-1]
            if failure == 'protocol': last['imap_commands']['ERROR'] = 1
            elif failure == 'memory': last['pss_kib'] = 81*1024
            else: last['cpu_seconds'] = 11
            self.assertFalse(measurement_result(report, 5)['budget_passed'])


if __name__ == '__main__':
    unittest.main()
